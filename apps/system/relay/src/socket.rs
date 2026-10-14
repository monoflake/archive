//! A WebSocket as the mesh and the browser use one, whichever end opened it: axum's for what this
//! relay accepts, tungstenite's for what it dials. Either is held as `Heard`, which drops it once
//! nothing has been heard on it for `SILENCE`. The mesh takes one apart into its `Halves`, so a
//! write waiting on the other end never stops the reading that end is waiting on.

use axum::extract::ws::{Message as Accepted, WebSocket};
use bytes::Bytes;
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use std::future::Future;
use std::os::fd::{AsFd, OwnedFd};
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

	/// Whether a write was given up for waiting too long, rather than failing.
	pub fn stalled(&self) -> bool {
		self.0.is::<Stalled>()
	}
}

/// A socket that has carried nothing, not even a pong, for ninety seconds -- thirty pings to a
/// neighbor, three heartbeats to a browser -- is gone. A write that has waited as long is given up.
pub const SILENCE: Duration = Duration::from_secs(90);

#[derive(Debug, thiserror::Error)]
#[error("nothing heard for {0:?}")]
pub struct Silent(pub Duration);

#[derive(Debug, thiserror::Error)]
#[error("a write waited {0:?}")]
pub struct Stalled(pub Duration);

/// A frame, of what this relay reads: text, a compressed frame, a pong, or the other end gone.
/// Pings are answered beneath.
#[derive(Debug, PartialEq, Eq)]
pub enum Received {
	Text(String),
	/// A frame the mesh compressed; see `compression`.
	Binary(Bytes),
	/// The answer to a ping, carrying back what the ping carried.
	Pong(Bytes),
	Closed,
	/// A ping: proof of life, and nothing more.
	Other,
}

/// A frame, of what this relay writes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Frame {
	Text(String),
	/// A frame compressed for a neighbor that reads them.
	Binary(Bytes),
	/// Carrying what the pong is to carry back.
	Ping(Bytes),
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

/// The half of a socket that reads.
pub trait Reader: Send {
	/// Cancel-safe, so it can wait in a `select!` beside the updates.
	fn receive(&mut self) -> impl Future<Output = Result<Received, SocketError>> + Send;

	/// The kernel's round trip for the connection beneath, where it can be read; see `tcp`.
	fn round_trip(&self) -> Option<Duration> {
		None
	}
}

/// The half of a socket that writes, in a task of its own.
pub trait Writer: Send + 'static {
	fn send(&mut self, frame: Frame) -> impl Future<Output = Result<(), SocketError>> + Send;
}

/// A socket taken apart, each half usable while the other waits.
pub trait Halves: Send {
	type Reader: Reader;
	type Writer: Writer;

	fn halves(self) -> (Self::Reader, Self::Writer);
}

impl From<std::io::Error> for SocketError {
	fn from(error: std::io::Error) -> Self {
		failed(error)
	}
}

fn failed(error: impl std::error::Error + Send + Sync + 'static) -> SocketError {
	SocketError(Box::new(error))
}

fn accepted(next: Option<Result<Accepted, axum::Error>>) -> Result<Received, SocketError> {
	Ok(match next {
		None | Some(Ok(Accepted::Close(_))) => Received::Closed,
		Some(Ok(Accepted::Text(text))) => Received::Text(text.as_str().to_owned()),
		Some(Ok(Accepted::Binary(bytes))) => Received::Binary(bytes),
		Some(Ok(Accepted::Pong(payload))) => Received::Pong(payload),
		Some(Ok(_)) => Received::Other,
		Some(Err(error)) => return Err(failed(error)),
	})
}

fn dialed(
	next: Option<Result<Dialed, tokio_tungstenite::tungstenite::Error>>,
) -> Result<Received, SocketError> {
	Ok(match next {
		None | Some(Ok(Dialed::Close(_))) => Received::Closed,
		Some(Ok(Dialed::Text(text))) => Received::Text(text.as_str().to_owned()),
		Some(Ok(Dialed::Binary(bytes))) => Received::Binary(bytes),
		Some(Ok(Dialed::Pong(payload))) => Received::Pong(payload),
		Some(Ok(_)) => Received::Other,
		Some(Err(error)) => return Err(failed(error)),
	})
}

impl From<Frame> for Accepted {
	fn from(frame: Frame) -> Self {
		match frame {
			Frame::Text(text) => Self::Text(text.into()),
			Frame::Binary(bytes) => Self::Binary(bytes),
			Frame::Ping(payload) => Self::Ping(payload),
		}
	}
}

impl From<Frame> for Dialed {
	fn from(frame: Frame) -> Self {
		match frame {
			Frame::Text(text) => Self::Text(text.into()),
			Frame::Binary(bytes) => Self::Binary(bytes),
			Frame::Ping(payload) => Self::Ping(payload),
		}
	}
}

impl Socket for WebSocket {
	async fn send_text(&mut self, text: String) -> Result<(), SocketError> {
		self.send(Frame::Text(text).into()).await.map_err(failed)
	}

	async fn ping(&mut self, payload: Bytes) -> Result<(), SocketError> {
		self.send(Frame::Ping(payload).into()).await.map_err(failed)
	}

	async fn receive(&mut self) -> Result<Received, SocketError> {
		accepted(self.recv().await)
	}
}

impl Reader for SplitStream<WebSocket> {
	async fn receive(&mut self) -> Result<Received, SocketError> {
		accepted(self.next().await)
	}
}

impl Writer for SplitSink<WebSocket, Accepted> {
	async fn send(&mut self, frame: Frame) -> Result<(), SocketError> {
		SinkExt::send(self, frame.into()).await.map_err(failed)
	}
}

/// Axum hands over an accepted socket with its stream out of reach, so it has no round trip.
impl Halves for WebSocket {
	type Reader = SplitStream<WebSocket>;
	type Writer = SplitSink<WebSocket, Accepted>;

	fn halves(self) -> (Self::Reader, Self::Writer) {
		let (writer, reader) = self.split();
		(reader, writer)
	}
}

pub type Dial = WebSocketStream<MaybeTlsStream<TcpStream>>;

impl Socket for Dial {
	async fn send_text(&mut self, text: String) -> Result<(), SocketError> {
		self.send(Frame::Text(text).into()).await.map_err(failed)
	}

	async fn ping(&mut self, payload: Bytes) -> Result<(), SocketError> {
		self.send(Frame::Ping(payload).into()).await.map_err(failed)
	}

	async fn receive(&mut self) -> Result<Received, SocketError> {
		dialed(self.next().await)
	}

	/// Read on the dialed socket alone: axum hands over an accepted one with its stream out of reach.
	fn round_trip(&self) -> Option<Duration> {
		match self.get_ref() {
			MaybeTlsStream::Plain(stream) => crate::tcp::round_trip(stream),
			_ => None,
		}
	}
}

/// The reading half of a dialed socket, with a handle of its own on the connection beneath to read
/// the kernel's round trip from, closed with it.
pub struct DialReader {
	stream: SplitStream<Dial>,
	connection: Option<OwnedFd>,
}

impl Reader for DialReader {
	async fn receive(&mut self) -> Result<Received, SocketError> {
		dialed(self.stream.next().await)
	}

	fn round_trip(&self) -> Option<Duration> {
		self.connection.as_ref().and_then(crate::tcp::round_trip)
	}
}

impl Writer for SplitSink<Dial, Dialed> {
	async fn send(&mut self, frame: Frame) -> Result<(), SocketError> {
		SinkExt::send(self, frame.into()).await.map_err(failed)
	}
}

impl Halves for Dial {
	type Reader = DialReader;
	type Writer = SplitSink<Dial, Dialed>;

	fn halves(self) -> (Self::Reader, Self::Writer) {
		let connection = match self.get_ref() {
			MaybeTlsStream::Plain(stream) => stream.as_fd().try_clone_to_owned().ok(),
			_ => None,
		};
		let (writer, stream) = self.split();
		(DialReader { stream, connection }, writer)
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

impl<S> Heard<S> {
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

/// `waiting`, or `over` once `deadline` passes.
async fn by<T, E: std::error::Error + Send + Sync + 'static>(
	deadline: Instant,
	over: E,
	waiting: impl Future<Output = Result<T, SocketError>>,
) -> Result<T, SocketError> {
	tokio::time::timeout_at(deadline, waiting).await.unwrap_or_else(|_| Err(failed(over)))
}

impl<S: Socket> Socket for Heard<S> {
	async fn send_text(&mut self, text: String) -> Result<(), SocketError> {
		by(self.deadline(), Silent(self.silence), self.socket.send_text(text)).await
	}

	async fn ping(&mut self, payload: Bytes) -> Result<(), SocketError> {
		by(self.deadline(), Silent(self.silence), self.socket.ping(payload)).await
	}

	async fn receive(&mut self) -> Result<Received, SocketError> {
		let received = by(self.deadline(), Silent(self.silence), self.socket.receive()).await?;
		self.heard = Instant::now();
		Ok(received)
	}

	fn round_trip(&self) -> Option<Duration> {
		self.socket.round_trip()
	}
}

/// Taken apart, the reading ends once nothing has been heard for the silence, and so does the
/// conversation; a write still waiting then goes with it. A write that has waited the silence while
/// the other end is still heard is given up on its own.
impl<S: Halves> Halves for Heard<S> {
	type Reader = Heard<S::Reader>;
	type Writer = Bounded<S::Writer>;

	fn halves(self) -> (Self::Reader, Self::Writer) {
		let (reader, writer) = self.socket.halves();
		let reader = Heard { socket: reader, silence: self.silence, heard: self.heard };
		(reader, Bounded { writer, patience: self.silence })
	}
}

impl<R: Reader> Reader for Heard<R> {
	async fn receive(&mut self) -> Result<Received, SocketError> {
		let received = by(self.deadline(), Silent(self.silence), self.socket.receive()).await?;
		self.heard = Instant::now();
		Ok(received)
	}

	fn round_trip(&self) -> Option<Duration> {
		self.socket.round_trip()
	}
}

/// A writer whose every write is given up once it has waited `patience`.
pub struct Bounded<W> {
	writer: W,
	patience: Duration,
}

impl<W: Writer> Writer for Bounded<W> {
	async fn send(&mut self, frame: Frame) -> Result<(), SocketError> {
		let deadline = Instant::now() + self.patience;
		by(deadline, Stalled(self.patience), self.writer.send(frame)).await
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
