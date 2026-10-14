//! What a conversation hands its socket's writer, and the writer's task.

use super::{MeshError, Message};
use crate::compression::Packer;
use crate::relay::Relay;
use crate::socket::{Frame, SocketError, Writer};
use bytes::Bytes;
use std::sync::Arc;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};

/// How many frames a socket's writer holds waiting. Past it a frame is dropped rather than waited
/// for, since waiting would stop the reading; what a neighbor misses so, the next comparison sends.
pub(super) const QUEUE: usize = 64;

/// A frame for the writer, and the version of this node's own it carries, told once written.
pub(super) struct Outgoing {
	pub(super) frame: Frame,
	pub(super) own: Option<u64>,
}

/// The writer's queue as a conversation hands it frames: at once, or not at all.
pub(super) struct Outbox {
	pub(super) queue: mpsc::Sender<Outgoing>,
	pub(super) dropped: usize,
}

impl Outbox {
	/// `message` queued, carrying this node's own version `own`; written out only while there is
	/// room, and dropped otherwise.
	pub(super) fn send(&mut self, message: &Message, own: Option<u64>) -> Result<(), MeshError> {
		let Ok(room) = self.queue.try_reserve() else {
			self.dropped += 1;
			return Ok(());
		};
		room.send(Outgoing { frame: Frame::Text(serde_json::to_string(message)?), own });
		Ok(())
	}

	/// A ping queued; one that finds the queue full is skipped, its figure the queue's.
	pub(super) fn ping(&mut self, payload: Bytes) {
		if let Ok(room) = self.queue.try_reserve() {
			room.send(Outgoing { frame: Frame::Ping(payload), own: None });
		}
	}

	/// How many messages were dropped since this was last asked.
	pub(super) fn dropped(&mut self) -> usize {
		std::mem::take(&mut self.dropped)
	}
}

/// The writer's task, ended with the conversation however it ends.
pub(super) struct Writing(pub(super) JoinHandle<Result<(), SocketError>>);

impl Writing {
	/// Once the writer has written what it was handed and its queue is closed.
	pub(super) async fn finished(mut self) -> Result<(), MeshError> {
		written((&mut self.0).await)
	}
}

impl Drop for Writing {
	fn drop(&mut self) {
		self.0.abort();
	}
}

/// How the writer ended, a panic in it carried on as one.
pub(super) fn written(joined: Result<Result<(), SocketError>, JoinError>) -> Result<(), MeshError> {
	match joined {
		Ok(written) => Ok(written?),
		Err(failed) => std::panic::resume_unwind(failed.into_panic()),
	}
}

/// Writes what the conversation queues, in order, until the queue closes or a write fails, each
/// text frame compressed where `packer` is given; this node's own version is told to `peer` once it
/// is written.
pub(super) async fn write<W: Writer>(
	relay: Arc<Relay>,
	peer: String,
	mut writer: W,
	mut packer: Option<Packer>,
	mut queue: mpsc::Receiver<Outgoing>,
) -> Result<(), SocketError> {
	while let Some(Outgoing { frame, own }) = queue.recv().await {
		let frame = match (frame, &mut packer) {
			(Frame::Text(text), Some(packer)) => Frame::Binary(packer.pack(text.as_bytes())?.into()),
			(frame, _) => frame,
		};
		writer.send(frame).await?;
		if let Some(version) = own {
			relay.told(&peer, version);
		}
	}
	Ok(())
}
