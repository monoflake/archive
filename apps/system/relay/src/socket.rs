//! A WebSocket as the mesh and the browser use one, whichever end opened it: axum's for what this
//! relay accepts, tungstenite's for what it dials. Either is held as `Heard`, which drops it once
//! nothing has been heard on it for `SILENCE`.

use axum::extract::ws::{Message as Accepted, WebSocket};
use bytes::Bytes;
use futures_util::{SinkExt, StreamExt};
use std::future::Future;
use std::time::Duration;
use tokio::net::TcpStream;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::Message as Dialed;
use tokio_tungstenite::{MaybeTlsStream, WebSocketStream};

#[derive(Debug, thiserror::Error)]
#[error("the socket: {0}")]
pub struct SocketError(#[source] Box<dyn std::error::Error + Send + Sync>);

impl SocketError {
	/// Whether the socket was dropped for its silence, rather than failing.
	pub fn silent(&self) -> bool {
		self.0.is::<Silent>()
	}
}

/// A socket that has carried nothing, not even a pong, for ninety seconds -- thirty pings to a
/// neighbor, three heartbeats to a browser -- is gone.
pub const SILENCE: Duration = Duration::from_secs(90);

#[derive(Debug, thiserror::Error)]
#[error("nothing heard for {0:?}")]
pub struct Silent(pub Duration);

/// A frame, of what this relay reads: text, a pong, or the other end gone. Pings are answered
/// beneath.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
	Text(String),
	/// The answer to a ping, carrying back what the ping carried.
	Pong(Bytes),
	Closed,
	/// A ping or binary: proof of life, and nothing more.
	Other,
}

pub trait Socket: Send {
	fn send_text(&mut self, text: String) -> impl Future<Output = Result<(), SocketError>> + Send;
	fn ping(&mut self, payload: Bytes) -> impl Future<Output = Result<(), SocketError>> + Send;
	/// Cancel-safe, so it can wait in a `select!` beside the updates.
	fn receive(&mut self) -> impl Future<Output = Result<Received, SocketError>> + Send;

	/// The kernel's round trip for the connection beneath, where it can be read; see `tcp`.
	fn round_trip(&self) -> Option<Duration> {
		None
	}
}

impl Socket for WebSocket {
	async fn send_text(&mut self, text: String) -> Result<(), SocketError> {
		self.send(Accepted::Text(text.into())).await.map_err(|error| SocketError(error.into()))
	}

	async fn ping(&mut self, payload: Bytes) -> Result<(), SocketError> {
		self.send(Accepted::Ping(payload)).await.map_err(|error| SocketError(error.into()))
	}

	async fn receive(&mut self) -> Result<Received, SocketError> {
		Ok(match self.recv().await {
			None | Some(Ok(Accepted::Close(_))) => Received::Closed,
			Some(Ok(Accepted::Text(text))) => Received::Text(text.as_str().to_owned()),
			Some(Ok(Accepted::Pong(payload))) => Received::Pong(payload),
			Some(Ok(_)) => Received::Other,
			Some(Err(error)) => return Err(SocketError(error.into())),
		})
	}
}

pub type Dial = WebSocketStream<MaybeTlsStream<TcpStream>>;

impl Socket for Dial {
	async fn send_text(&mut self, text: String) -> Result<(), SocketError> {
		self.send(Dialed::Text(text.into())).await.map_err(|error| SocketError(error.into()))
	}

	async fn ping(&mut self, payload: Bytes) -> Result<(), SocketError> {
		self.send(Dialed::Ping(payload)).await.map_err(|error| SocketError(error.into()))
	}

	async fn receive(&mut self) -> Result<Received, SocketError> {
		Ok(match self.next().await {
			None | Some(Ok(Dialed::Close(_))) => Received::Closed,
			Some(Ok(Dialed::Text(text))) => Received::Text(text.as_str().to_owned()),
			Some(Ok(Dialed::Pong(payload))) => Received::Pong(payload),
			Some(Ok(_)) => Received::Other,
			Some(Err(error)) => return Err(SocketError(error.into())),
		})
	}

	/// Read on the dialed socket alone: axum hands over an accepted one with its stream out of reach.
	fn round_trip(&self) -> Option<Duration> {
		match self.get_ref() {
			MaybeTlsStream::Plain(stream) => crate::tcp::smoothed(stream),
			_ => None,
		}
	}
}

/// A socket whose every wait, a write's as much as a read's, ends once nothing has been heard on it
/// for its silence. A peer gone without closing stops taking what is written; once the kernel's
/// buffer is full a write waits on it until TCP gives up, a quarter of an hour, and a check made
/// between writes is never reached.
pub struct Heard<S> {
	socket: S,
	silence: Duration,
	heard: Instant,
}

impl<S: Socket> Heard<S> {
	pub fn new(socket: S) -> Self {
		Self::within(socket, SILENCE)
	}

	/// One dropped after `silence` rather than `SILENCE`, as a test holds one.
	pub fn within(socket: S, silence: Duration) -> Self {
		Self { socket, silence, heard: Instant::now() }
	}

	fn deadline(&self) -> Instant {
		self.heard + self.silence
	}
}

/// `waiting`, or `Silent` once `deadline` passes.
async fn by<T>(
	deadline: Instant,
	silence: Duration,
	waiting: impl Future<Output = Result<T, SocketError>>,
) -> Result<T, SocketError> {
	tokio::time::timeout_at(deadline, waiting)
		.await
		.unwrap_or_else(|_| Err(SocketError(Box::new(Silent(silence)))))
}

impl<S: Socket> Socket for Heard<S> {
	async fn send_text(&mut self, text: String) -> Result<(), SocketError> {
		by(self.deadline(), self.silence, self.socket.send_text(text)).await
	}

	async fn ping(&mut self, payload: Bytes) -> Result<(), SocketError> {
		by(self.deadline(), self.silence, self.socket.ping(payload)).await
	}

	async fn receive(&mut self) -> Result<Received, SocketError> {
		let received = by(self.deadline(), self.silence, self.socket.receive()).await?;
		self.heard = Instant::now();
		Ok(received)
	}

	fn round_trip(&self) -> Option<Duration> {
		self.socket.round_trip()
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use tokio::sync::mpsc;

	struct Quiet(mpsc::UnboundedReceiver<Received>);

	impl Socket for Quiet {
		async fn send_text(&mut self, _: String) -> Result<(), SocketError> {
			Ok(())
		}

		async fn ping(&mut self, _: Bytes) -> Result<(), SocketError> {
			Ok(())
		}

		async fn receive(&mut self) -> Result<Received, SocketError> {
			Ok(self.0.recv().await.unwrap_or(Received::Closed))
		}
	}

	#[tokio::test]
	async fn the_silence_counts_from_the_last_frame_heard() {
		let silence = Duration::from_millis(300);
		let (feed, incoming) = mpsc::unbounded_channel();
		let mut socket = Heard::within(Quiet(incoming), silence);
		tokio::time::sleep(Duration::from_millis(200)).await;
		feed.send(Received::Other).unwrap();
		assert_eq!(socket.receive().await.unwrap(), Received::Other);
		let heard = Instant::now();
		let error = socket.receive().await.unwrap_err();
		assert!(error.silent());
		assert!(heard.elapsed() >= silence);
		drop(feed);
	}
}
