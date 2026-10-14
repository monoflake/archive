//! The relays' mesh: a socket to every other node's relay. Both ends open with the versions they
//! hold and send each other what the other lacks; after that a relay sends its own node's snapshot
//! the moment it changes, a beat every round, and every other node's snapshot when a neighbor's
//! comparison shows it behind. The kernel's round trip on the socket it dialed, or else its pings
//! timed, is what its minutes carry to each neighbor. The runs and the minutes travel the same way,
//! each by their own versions, and a node's fine readings only to a neighbor holding a lease. See
//! spec/architecture/relay.md.

use crate::cluster::{Carried, Versions};
use crate::config::Peer;
use crate::relay::{Relay, Update, VERSION};
use crate::runs::Batch;
use crate::socket::{Dial, Frame, Halves, Heard, Reader, Received, SocketError, Writer};
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, InvalidHeaderValue};
use axum::serve::{Listener, ListenerExt};
use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::task::{JoinError, JoinHandle};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

/// Where a relay accepts its neighbors.
pub const PATH: &str = "/mesh";

/// How often a relay tells a neighbor what it holds, and so the most a snapshot or a beat waits at
/// each hop past its own node.
pub const COMPARISON: Duration = Duration::from_secs(2);

/// Cloudflare and NATs close a socket idle for long; a ping keeps it from looking idle. A browser's
/// socket is pinged this often; a neighbor's, every `PING`.
pub const HEARTBEAT: Duration = Duration::from_secs(30);

/// How often a relay pings each neighbor and times the pong, and reads the kernel's round trip
/// where it can, the first as the socket opens.
/// Its own round is every `own::EVERY`, so a faster ping times a figure no round carries; a slower
/// one leaves the figure a round behind. See spec/architecture/relay.md, "The round trip to each
/// neighbor".
pub const PING: Duration = crate::own::EVERY;

/// How long the other end has to say who it is.
const INTRODUCTION: Duration = Duration::from_secs(10);

const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(60);

/// What one relay sends another. `runs` is absent from a relay older than the runs, which is
/// never sent `Runs`, a kind it cannot read; so the contract's version stands. `minutes` is the
/// same of the minutes, `beats` of `Beat` and `watch` of `Watch` and `Readings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
	/// Sent first by both ends: the contract's version, who it is, and what it holds.
	Hello {
		version: u32,
		node: String,
		versions: Versions,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		runs: Option<Versions>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		minutes: Option<Versions>,
		/// Whether it hears a node by its beats, and so needs no new version every round.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		beats: Option<bool>,
		/// Whether it lends and reads fine readings.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		watch: Option<bool>,
	},
	/// What it holds, every comparison; answered with what it lacks. `beats` is each node's newest
	/// beat held, so a node heard only through a neighbor stays live.
	Versions {
		versions: Versions,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		runs: Option<Versions>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		minutes: Option<Versions>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		beats: Option<Versions>,
	},
	/// A node's snapshot, newer than what the other was last known to hold.
	Node { node: String, state: Carried },
	/// Some of a node's runs, which the other was last known to lack.
	Runs(Batch),
	/// Some of a node's minutes, which the other was last known to lack.
	Minutes(crate::history::Batch),
	/// That its node is alive, at its clock in milliseconds: every round, and nothing else.
	Beat { node: String, at: u64 },
	/// A page on the sender asks this node's fine readings, for `lease` seconds.
	Watch { lease: u64 },
	/// The node's fine readings of a round, to a neighbor holding a lease.
	Readings { node: String, sample: serde_json::Value },
}

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
	#[error(transparent)]
	Socket(#[from] SocketError),
	#[error("dialing: {0}")]
	Dial(#[from] tungstenite::Error),
	#[error("no answer within {0:?}")]
	Slow(Duration),
	#[error("a message this relay cannot read: {0}")]
	Malformed(#[from] serde_json::Error),
	#[error("it speaks version {0} of the mesh")]
	Version(u32),
	#[error("it opened with something other than hello")]
	Unintroduced,
	#[error("it says it is `{0}`")]
	Stranger(String),
	#[error("it says it is `{0}`, a node retired")]
	Retired(String),
	#[error("it closed the socket")]
	Closed,
}

/// Whether a request carries the mesh's secret, compared in time independent of where the first
/// difference is.
pub fn admitted(headers: &HeaderMap, secret: &str) -> bool {
	let given = headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.unwrap_or_default();
	!secret.is_empty()
		&& given.len() == secret.len()
		&& given.bytes().zip(secret.bytes()).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

/// One of the sockets to a neighbor -- two relays that each dial the other hold two. Only the first
/// open sends unasked, so nothing goes twice; the other takes over when it closes.
struct Speaking<'a> {
	relay: &'a Relay,
	peer: &'a str,
	conversation: u64,
}

impl Speaking<'_> {
	fn speaks(&self) -> bool {
		self.relay.speaks(self.peer, self.conversation)
	}
}

impl Drop for Speaking<'_> {
	fn drop(&mut self) {
		self.relay.hush(self.peer, self.conversation);
		self.relay.parted(self.conversation);
	}
}

/// How many frames a socket's writer holds waiting. Past it a frame is dropped rather than waited
/// for, since waiting would stop the reading; what a neighbor misses so, the next comparison sends.
const QUEUE: usize = 64;

/// A frame for the writer, and the version of this node's own it carries, told once written.
struct Outgoing {
	frame: Frame,
	own: Option<u64>,
}

/// The writer's queue as a conversation hands it frames: at once, or not at all.
struct Outbox {
	queue: mpsc::Sender<Outgoing>,
	dropped: usize,
}

impl Outbox {
	/// `message` queued, carrying this node's own version `own`; written out only while there is
	/// room, and dropped otherwise.
	fn send(&mut self, message: &Message, own: Option<u64>) -> Result<(), MeshError> {
		let Ok(room) = self.queue.try_reserve() else {
			self.dropped += 1;
			return Ok(());
		};
		room.send(Outgoing { frame: Frame::Text(serde_json::to_string(message)?), own });
		Ok(())
	}

	/// A ping queued; one that finds the queue full is skipped, its figure the queue's.
	fn ping(&mut self, payload: Bytes) {
		if let Ok(room) = self.queue.try_reserve() {
			room.send(Outgoing { frame: Frame::Ping(payload), own: None });
		}
	}

	/// How many messages were dropped since this was last asked.
	fn dropped(&mut self) -> usize {
		std::mem::take(&mut self.dropped)
	}
}

/// The writer's task, ended with the conversation however it ends.
struct Writing(JoinHandle<Result<(), SocketError>>);

impl Writing {
	/// Once the writer has written what it was handed and its queue is closed.
	async fn finished(mut self) -> Result<(), MeshError> {
		written((&mut self.0).await)
	}
}

impl Drop for Writing {
	fn drop(&mut self) {
		self.0.abort();
	}
}

/// How the writer ended, a panic in it carried on as one.
fn written(joined: Result<Result<(), SocketError>, JoinError>) -> Result<(), MeshError> {
	match joined {
		Ok(written) => Ok(written?),
		Err(failed) => std::panic::resume_unwind(failed.into_panic()),
	}
}

/// Writes what the conversation queues, in order, until the queue closes or a write fails; this
/// node's own version is told to `peer` once it is written.
async fn write<W: Writer>(
	relay: Arc<Relay>,
	peer: String,
	mut writer: W,
	mut queue: mpsc::Receiver<Outgoing>,
) -> Result<(), SocketError> {
	while let Some(Outgoing { frame, own }) = queue.recv().await {
		writer.send(frame).await?;
		if let Some(version) = own {
			relay.told(&peer, version);
		}
	}
	Ok(())
}

/// One socket to a neighbor, from the introductions until it closes or falls silent, either end
/// alike. `expected` is the neighbor dialed, and none for one that dialed here. The socket is read
/// here and written by a task of its own, so a write waiting on the neighbor never stops the
/// reading that the neighbor, waiting on its own write, needs to go on.
pub async fn converse<S: Halves>(
	relay: Arc<Relay>,
	expected: Option<&str>,
	socket: Heard<S>,
) -> Result<(), MeshError> {
	// Subscribed before saying what is held, so nothing taken meanwhile is missed.
	let mut updates = relay.subscribe();
	let mut batches = relay.subscribe_runs();
	let mut minutes = relay.subscribe_minutes();
	let mut beats = relay.subscribe_beats();
	let mut asking = relay.subscribe_asking();
	let mut lent = relay.subscribe_lent();
	let (mut reader, mut writer) = socket.halves();
	let hello = Message::Hello {
		version: VERSION,
		node: relay.node().to_owned(),
		versions: relay.versions(),
		runs: Some(relay.run_versions()),
		minutes: Some(relay.minute_versions()),
		beats: Some(true),
		watch: Some(true),
	};
	writer.send(Frame::Text(serde_json::to_string(&hello)?)).await?;

	let theirs = tokio::time::timeout(INTRODUCTION, introduced(&mut reader))
		.await
		.map_err(|_| MeshError::Slow(INTRODUCTION))??;
	let (peer, versions, runs, theirs_minutes, hears_beats, lends) = match theirs {
		Message::Hello { version, .. } if version != VERSION => {
			return Err(MeshError::Version(version));
		}
		Message::Hello { node, versions, runs, minutes, beats, watch, .. } => {
			(node, versions, runs, minutes, beats == Some(true), watch == Some(true))
		}
		Message::Versions { .. }
		| Message::Node { .. }
		| Message::Runs(_)
		| Message::Minutes(_)
		| Message::Beat { .. }
		| Message::Watch { .. }
		| Message::Readings { .. } => {
			return Err(MeshError::Unintroduced);
		}
	};
	if peer == relay.node() || expected.is_some_and(|expected| expected != peer) {
		return Err(MeshError::Stranger(peer));
	}
	// A node retired leaves the mesh both ways, never by its record going missing -- RFC qcyb3.
	if relay.is_retired(&peer) {
		return Err(MeshError::Retired(peer));
	}

	let speaking = Speaking { relay: &relay, peer: &peer, conversation: relay.conversation() };
	relay.met(speaking.conversation, hears_beats);
	let (queue, queued) = mpsc::channel(QUEUE);
	let mut outbox = Outbox { queue, dropped: 0 };
	let mut writing = Writing(tokio::spawn(write(relay.clone(), peer.clone(), writer, queued)));
	// A neighbor that said nothing of runs or minutes is older than them; see `Message`.
	let (reads_runs, reads_minutes) = (runs.is_some(), theirs_minutes.is_some());
	behind(&relay, &mut outbox, &versions, &peer)?;
	if let Some(runs) = runs {
		runs_behind(&relay, &mut outbox, &runs, &peer).await?;
	}
	if let Some(theirs) = theirs_minutes {
		minutes_behind(&relay, &mut outbox, &theirs, &peer).await?;
	}

	let mut comparison = tokio::time::interval_at(Instant::now() + COMPARISON, COMPARISON);
	let mut ping = tokio::time::interval(PING);
	let origin = Instant::now();
	loop {
		tokio::select! {
			received = reader.receive() => {
				match received? {
					Received::Text(text) => match serde_json::from_str(&text)? {
						Message::Node { node, state } => {
							relay.take(&node, state, Some(&peer));
						}
						Message::Versions { versions, runs, minutes, beats } => {
							for (node, at) in beats.unwrap_or_default() {
								relay.beaten(&node, at, Some(&peer));
							}
							behind(&relay, &mut outbox, &versions, &peer)?;
							if let Some(runs) = runs {
								runs_behind(&relay, &mut outbox, &runs, &peer).await?;
							}
							if let Some(minutes) = minutes {
								minutes_behind(&relay, &mut outbox, &minutes, &peer).await?;
							}
						}
						Message::Runs(batch) => {
							relay.take_runs(batch);
						}
						Message::Minutes(batch) => {
							relay.take_minutes(batch);
						}
						Message::Beat { node, at } => relay.beaten(&node, at, Some(&peer)),
						Message::Watch { lease } => relay.leased(&peer, Duration::from_secs(lease)),
						Message::Readings { node, sample } => {
							if node != relay.node() {
								relay.heard_readings(&node, sample);
							}
						}
						Message::Hello { .. } => {}
					},
					Received::Pong(payload) => {
						if let Some(round_trip) = round_trip(origin, &payload, Instant::now()) {
							relay.timed(&peer, round_trip);
						}
					}
					Received::Closed => break,
					Received::Other => {}
				}
			}
			update = updates.recv() => match update {
				Ok(Update { node, held }) => {
					if node == relay.node() && speaking.speaks() {
						let state = Carried { version: held.version, snapshot: held.snapshot };
						outbox.send(&Message::Node { node, state }, Some(held.version))?;
					}
				}
				// What it missed, the next comparison finds.
				Err(RecvError::Lagged(_)) => {}
				Err(RecvError::Closed) => return Ok(()),
			},
			batch = batches.recv() => match batch {
				Ok(batch) => {
					if batch.node == relay.node() && reads_runs && speaking.speaks() {
						outbox.send(&Message::Runs(batch), None)?;
					}
				}
				Err(RecvError::Lagged(_)) => {}
				Err(RecvError::Closed) => return Ok(()),
			},
			batch = minutes.recv() => match batch {
				Ok(batch) => {
					if batch.node == relay.node() && reads_minutes && speaking.speaks() {
						outbox.send(&Message::Minutes(batch), None)?;
					}
				}
				Err(RecvError::Lagged(_)) => {}
				Err(RecvError::Closed) => return Ok(()),
			},
			beat = beats.recv() => match beat {
				Ok(at) => {
					if hears_beats && speaking.speaks() {
						outbox.send(&Message::Beat { node: relay.node().to_owned(), at }, None)?;
					}
				}
				// The next beat says the same, newer.
				Err(RecvError::Lagged(_)) => {}
				Err(RecvError::Closed) => return Ok(()),
			},
			asked = asking.recv() => match asked {
				Ok(node) => {
					if node == peer && lends && speaking.speaks() {
						outbox.send(&Message::Watch { lease: crate::relay::LEASE.as_secs() }, None)?;
					}
				}
				// A page asks again before its lease lapses.
				Err(RecvError::Lagged(_)) => {}
				Err(RecvError::Closed) => return Ok(()),
			},
			sample = lent.recv() => match sample {
				Ok(sample) => {
					if lends && relay.leased_to(&peer) && speaking.speaks() {
						let readings = Message::Readings { node: relay.node().to_owned(), sample };
						outbox.send(&readings, None)?;
					}
				}
				Err(RecvError::Lagged(_)) => {}
				Err(RecvError::Closed) => return Ok(()),
			},
			_ = comparison.tick() => {
				// One retired while its socket was open is dropped within a comparison.
				if relay.is_retired(&peer) {
					return Err(MeshError::Retired(peer.clone()));
				}
				let dropped = outbox.dropped();
				if dropped > 0 {
					eprintln!("relay: {peer} behind; dropped {dropped}, the comparison sends what it lacks");
				}
				if speaking.speaks() {
					let (runs, minutes) = (Some(relay.run_versions()), Some(relay.minute_versions()));
					let beats = Some(relay.beats());
					let versions = Message::Versions { versions: relay.versions(), runs, minutes, beats };
					outbox.send(&versions, None)?;
				}
			}
			_ = ping.tick() => {
				outbox.ping(stamp(origin, Instant::now()));
				if let Some(round_trip) = reader.round_trip() {
					relay.measured(&peer, round_trip);
				}
			}
			joined = &mut writing.0 => return written(joined),
		}
	}
	// Closed by the other end: what was queued is written, as far as it still takes it.
	drop(outbox);
	writing.finished().await
}

/// A ping's payload: when it was sent, in microseconds since the socket's `origin`. The pong
/// carries it back, so nothing is kept per ping, and a pong later than the next ping still times
/// its own.
fn stamp(origin: Instant, now: Instant) -> Bytes {
	let sent = u64::try_from(now.duration_since(origin).as_micros()).unwrap_or(u64::MAX);
	Bytes::copy_from_slice(&sent.to_be_bytes())
}

/// The round trip a pong closes, or none for a pong to no ping of this socket's: another shape, or
/// sent later than `now`.
fn round_trip(origin: Instant, payload: &[u8], now: Instant) -> Option<Duration> {
	let sent = u64::from_be_bytes(payload.try_into().ok()?);
	let elapsed = now.duration_since(origin);
	elapsed.checked_sub(Duration::from_micros(sent))
}

/// Queues for `peer`, holding `versions`, what it lacks.
fn behind(
	relay: &Relay,
	outbox: &mut Outbox,
	versions: &Versions,
	peer: &str,
) -> Result<(), MeshError> {
	for (node, state) in relay.lacking(versions, peer) {
		let own = (node == relay.node()).then_some(state.version);
		outbox.send(&Message::Node { node, state }, own)?;
	}
	Ok(())
}

/// Queues for `peer`, holding `theirs` of the runs, what it lacks. A file that cannot be read is
/// logged, and asked again at the next comparison; the socket stays.
async fn runs_behind(
	relay: &Relay,
	outbox: &mut Outbox,
	theirs: &Versions,
	peer: &str,
) -> Result<(), MeshError> {
	match relay.lacking_runs(theirs, peer).await {
		Ok(batches) => {
			for batch in batches {
				outbox.send(&Message::Runs(batch), None)?;
			}
		}
		Err(error) => eprintln!("relay: reading the runs for {peer}: {error}"),
	}
	Ok(())
}

/// Queues for `peer`, holding `theirs` of the minutes, what it lacks; a file that cannot be read is
/// logged, as the runs' is.
async fn minutes_behind(
	relay: &Relay,
	outbox: &mut Outbox,
	theirs: &Versions,
	peer: &str,
) -> Result<(), MeshError> {
	match relay.lacking_minutes(theirs, peer).await {
		Ok(batches) => {
			for batch in batches {
				outbox.send(&Message::Minutes(batch), None)?;
			}
		}
		Err(error) => eprintln!("relay: reading the minutes for {peer}: {error}"),
	}
	Ok(())
}

async fn introduced<R: Reader>(reader: &mut R) -> Result<Message, MeshError> {
	loop {
		match reader.receive().await? {
			Received::Text(text) => return Ok(serde_json::from_str(&text)?),
			Received::Closed => return Err(MeshError::Closed),
			Received::Pong(_) | Received::Other => {}
		}
	}
}

/// Opens a socket to `peer`'s relay, carrying the secret.
pub async fn dial(peer: &Peer, secret: &str) -> Result<Dial, MeshError> {
	let mut request = format!("ws://{}{PATH}", peer.address).into_client_request()?;
	let bearer = format!("Bearer {secret}")
		.parse()
		.map_err(|error: InvalidHeaderValue| tungstenite::Error::HttpFormat(error.into()))?;
	request.headers_mut().insert(AUTHORIZATION, bearer);
	// Without TCP_NODELAY a pong waits on the ACK of what went before it, a round trip more.
	let dialing = tokio_tungstenite::connect_async_with_config(request, None, true);
	let (socket, _) = tokio::time::timeout(INTRODUCTION, dialing)
		.await
		.map_err(|_| MeshError::Slow(INTRODUCTION))??;
	Ok(socket)
}

/// `listener`, its every connection without Nagle's delay, as `dial`'s are.
pub fn accepting(listener: TcpListener) -> impl Listener<Io = TcpStream, Addr = SocketAddr> {
	listener.tap_io(|stream: &mut TcpStream| {
		if let Err(error) = stream.set_nodelay(true) {
			eprintln!("relay: setting TCP_NODELAY on a connection: {error}");
		}
	})
}

/// Holds a socket to `peer` for as long as the relay runs, dialing again after each loss, waiting
/// twice as long each time it fails up to a minute. A neighbor down waits alone.
pub async fn keep(relay: Arc<Relay>, peer: Peer) {
	let mut wait = FIRST_RETRY;
	loop {
		match dial(&peer, relay.secret()).await {
			Ok(socket) => {
				eprintln!("relay: holding {}", peer.name);
				let opened = Instant::now();
				let ended = converse(relay.clone(), Some(&peer.name), Heard::new(socket)).await;
				match ended {
					Ok(()) => eprintln!("relay: {} closed", peer.name),
					Err(error) => eprintln!("relay: lost {}: {error}", peer.name),
				}
				if opened.elapsed() >= LAST_RETRY {
					wait = FIRST_RETRY;
				}
			}
			Err(error) => eprintln!("relay: dialing {} at {}: {error}", peer.name, peer.address),
		}
		tokio::time::sleep(wait).await;
		wait = (wait * 2).min(LAST_RETRY);
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use axum::http::HeaderValue;
	use std::sync::Mutex;
	use tokio::sync::mpsc;
	use tokio_tungstenite::MaybeTlsStream;

	fn relay(node: &str) -> Arc<Relay> {
		Relay::new(
			node.into(),
			"s3cret".into(),
			Default::default(),
			crate::runs::Store::memory().unwrap(),
		)
		.unwrap()
	}

	/// A socket whose other end is the test: what it is fed, it receives, and what it sends is kept.
	/// Given an `echo`, it answers its own pings, as the other end's socket does beneath.
	struct Fake {
		incoming: mpsc::UnboundedReceiver<Received>,
		sent: Arc<Mutex<Vec<Message>>>,
		echo: Option<mpsc::UnboundedSender<Received>>,
	}

	/// What a test feeds a socket, as its reading half.
	struct Fed(mpsc::UnboundedReceiver<Received>);

	impl Reader for Fed {
		async fn receive(&mut self) -> Result<Received, SocketError> {
			Ok(self.0.recv().await.unwrap_or(Received::Closed))
		}
	}

	/// Where a fake keeps what it is sent, as its writing half.
	struct Kept {
		sent: Arc<Mutex<Vec<Message>>>,
		echo: Option<mpsc::UnboundedSender<Received>>,
	}

	impl Writer for Kept {
		async fn send(&mut self, frame: Frame) -> Result<(), SocketError> {
			match frame {
				Frame::Text(text) => self.sent.lock().unwrap().push(serde_json::from_str(&text).unwrap()),
				Frame::Ping(payload) => {
					if let Some(echo) = &self.echo {
						let _ = echo.send(Received::Pong(payload));
					}
				}
			}
			Ok(())
		}
	}

	impl Halves for Fake {
		type Reader = Fed;
		type Writer = Kept;

		fn halves(self) -> (Fed, Kept) {
			(Fed(self.incoming), Kept { sent: self.sent, echo: self.echo })
		}
	}

	#[tokio::test]
	async fn meeting_sends_hello_and_then_exactly_what_the_other_lacks() {
		let relay = relay("rdu");
		let snapshot = Arc::new(serde_json::json!({}));
		for (node, version) in [("rdu", 9), ("tyo", 4), ("buf", 7), ("gvx", 2)] {
			relay.take(node, Carried { version, snapshot: snapshot.clone() }, None);
		}
		let (feed, incoming) = mpsc::unbounded_channel();
		let sent = Arc::new(Mutex::new(Vec::new()));
		let theirs = Versions::from([("tyo".into(), 6), ("buf".into(), 7), ("gvx".into(), 1)]);
		let hello = Message::Hello {
			version: VERSION,
			node: "tyo".into(),
			versions: theirs,
			runs: None,
			minutes: None,
			beats: None,
			watch: None,
		};
		feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
		drop(feed);

		let fake = Heard::new(Fake { incoming, sent: sent.clone(), echo: None });
		converse(relay.clone(), Some("tyo"), fake).await.unwrap();
		let sent = sent.lock().unwrap();
		assert!(matches!(&sent[0], Message::Hello { node, versions, .. }
			if node == "rdu" && *versions == relay.versions()));
		// Not tyo's own, however old it is here, and not buf, which it holds as new.
		let nodes: Vec<(&str, u64)> = sent[1..]
			.iter()
			.map(|message| match message {
				Message::Node { node, state } => (node.as_str(), state.version),
				other => panic!("sent {other:?}"),
			})
			.collect();
		assert_eq!(nodes, [("gvx", 2), ("rdu", 9)]);
	}

	/// tyo's hello, holding `runs` of the runs or older than them.
	async fn met(relay: &Arc<Relay>, runs: Option<Versions>) -> Vec<Message> {
		let (feed, incoming) = mpsc::unbounded_channel();
		let sent = Arc::new(Mutex::new(Vec::new()));
		let hello = Message::Hello {
			version: VERSION,
			node: "tyo".into(),
			versions: Versions::new(),
			runs,
			minutes: None,
			beats: None,
			watch: None,
		};
		feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
		drop(feed);
		let fake = Heard::new(Fake { incoming, sent: sent.clone(), echo: None });
		converse(relay.clone(), Some("tyo"), fake).await.unwrap();
		sent.lock().unwrap().clone()
	}

	#[tokio::test]
	async fn a_neighbor_is_sent_the_runs_it_lacks_and_one_older_than_runs_none() {
		let relay = relay("rdu");
		let row = |id: i64, version: u64| crate::runs::Row {
			version,
			event: crate::host::Event {
				id,
				app: "geo".into(),
				action: "deploy".into(),
				source: crate::host::Source { kind: "panel".into(), run: None, commit: None },
				image: None,
				outcome: "succeeded".into(),
				stage: None,
				detail: None,
				started_at: jiff::Timestamp::now().to_string(),
				finished_at: None,
			},
		};
		for (node, version) in [("buf", 7), ("tyo", 4)] {
			relay.take_runs(Batch { node: node.into(), above: 0, version, rows: vec![row(1, version)] });
		}

		let runs: Vec<(String, u64, usize)> = met(&relay, Some(Versions::from([("buf".into(), 3)])))
			.await
			.into_iter()
			.filter_map(|message| match message {
				Message::Runs(batch) => Some((batch.node, batch.above, batch.rows.len())),
				_ => None,
			})
			.collect();
		// buf above what tyo holds of it, and never tyo's own.
		assert_eq!(runs, [("buf".into(), 3, 1)]);

		let older = met(&relay, None).await;
		assert!(matches!(&older[0], Message::Hello { runs: Some(_), .. }));
		assert!(!older.iter().any(|message| matches!(message, Message::Runs(_))));
	}

	/// tyo's hello, holding `minutes` of the minutes or older than them.
	async fn met_minutes(relay: &Arc<Relay>, minutes: Option<Versions>) -> Vec<Message> {
		let (feed, incoming) = mpsc::unbounded_channel();
		let sent = Arc::new(Mutex::new(Vec::new()));
		let hello = Message::Hello {
			version: VERSION,
			node: "tyo".into(),
			versions: Versions::new(),
			runs: None,
			minutes,
			beats: None,
			watch: None,
		};
		feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
		drop(feed);
		let fake = Heard::new(Fake { incoming, sent: sent.clone(), echo: None });
		converse(relay.clone(), Some("tyo"), fake).await.unwrap();
		sent.lock().unwrap().clone()
	}

	#[tokio::test]
	async fn a_neighbor_is_sent_the_minutes_it_lacks_and_one_older_than_minutes_none() {
		let relay = relay("rdu");
		let minute = |version: u64, at: i64| crate::history::Row {
			version,
			minute: serde_json::from_value(serde_json::json!({ "at": at, "beats": 20 })).unwrap(),
		};
		for (node, version) in [("buf", 7), ("tyo", 4)] {
			let rows = vec![minute(version, 1_791_499_980_000)];
			relay.take_minutes(crate::history::Batch { node: node.into(), above: 0, version, rows });
		}
		let sent = met_minutes(&relay, Some(Versions::from([("buf".into(), 3)]))).await;
		assert!(matches!(&sent[0], Message::Hello { minutes: Some(minutes), .. }
			if minutes.get("buf") == Some(&7)));
		let minutes: Vec<(String, u64, usize)> = sent
			.into_iter()
			.filter_map(|message| match message {
				Message::Minutes(batch) => Some((batch.node, batch.above, batch.rows.len())),
				_ => None,
			})
			.collect();
		// buf above what tyo holds of it, and never tyo's own.
		assert_eq!(minutes, [("buf".into(), 3, 1)]);
		let older = met_minutes(&relay, None).await;
		assert!(!older.iter().any(|message| matches!(message, Message::Minutes(_))));
		// A minute not on a minute's start is no minute, and is not taken.
		let odd = vec![minute(9, 1_791_499_980_001)];
		relay.take_minutes(crate::history::Batch {
			node: "gvx".into(),
			above: 0,
			version: 9,
			rows: odd,
		});
		let lacking = relay.lacking_minutes(&Versions::new(), "buf").await.unwrap();
		assert!(lacking.iter().find(|batch| batch.node == "gvx").unwrap().rows.is_empty());
	}

	#[tokio::test]
	async fn a_neighbor_that_is_not_the_one_dialed_is_refused() {
		let relay = relay("rdu");
		for (said, expected) in [("buf", Some("tyo")), ("rdu", None)] {
			let (feed, incoming) = mpsc::unbounded_channel();
			let hello = Message::Hello {
				version: VERSION,
				node: said.into(),
				versions: Versions::new(),
				runs: None,
				minutes: None,
				beats: None,
				watch: None,
			};
			feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
			let fake = Heard::new(Fake { incoming, sent: Arc::default(), echo: None });
			let refused = converse(relay.clone(), expected, fake).await;
			assert!(matches!(refused, Err(MeshError::Stranger(named)) if named == said));
		}
	}

	#[tokio::test]
	async fn a_neighbor_held_as_retired_is_refused() {
		let relay = relay("rdu");
		relay.set_retired(std::collections::BTreeSet::from(["buf".to_owned()]));
		for (said, refused) in [("buf", true), ("tyo", false)] {
			let (feed, incoming) = mpsc::unbounded_channel();
			let hello = Message::Hello {
				version: VERSION,
				node: said.into(),
				versions: Versions::new(),
				runs: None,
				minutes: None,
				beats: None,
				watch: None,
			};
			feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
			feed.send(Received::Closed).unwrap();
			let fake = Heard::new(Fake { incoming, sent: Arc::default(), echo: None });
			let answered = converse(relay.clone(), None, fake).await;
			let named = matches!(&answered, Err(MeshError::Retired(named)) if named == said);
			assert_eq!(named, refused, "{said}: {answered:?}");
		}
	}

	#[tokio::test]
	async fn a_pong_times_the_round_trip_to_the_neighbor_that_sent_it() {
		let relay = relay("rdu");
		let (feed, incoming) = mpsc::unbounded_channel();
		let hello = Message::Hello {
			version: VERSION,
			node: "tyo".into(),
			versions: Versions::new(),
			runs: None,
			minutes: None,
			beats: None,
			watch: None,
		};
		feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
		let fake = Heard::new(Fake { incoming, sent: Arc::default(), echo: Some(feed) });
		let conversing = tokio::spawn(converse(relay.clone(), Some("tyo"), fake));
		let timed = async {
			while relay.round_trips().is_empty() {
				tokio::time::sleep(Duration::from_millis(5)).await;
			}
		};
		tokio::time::timeout(Duration::from_secs(5), timed).await.expect("the first ping, timed");
		conversing.abort();
		let round_trips = relay.round_trips();
		assert_eq!(round_trips.keys().collect::<Vec<_>>(), ["tyo"]);
		assert!((0.0..1000.0).contains(&round_trips["tyo"]));
	}

	/// A socket to a neighbor gone without closing: the hello goes, and every write after it waits on
	/// a buffer nobody drains, as one does once the kernel's is full.
	struct Stuck {
		incoming: mpsc::UnboundedReceiver<Received>,
	}

	/// The writing half of `Stuck`.
	struct Waiting {
		written: usize,
	}

	impl Writer for Waiting {
		async fn send(&mut self, _: Frame) -> Result<(), SocketError> {
			self.written += 1;
			if self.written > 1 {
				std::future::pending::<()>().await;
			}
			Ok(())
		}
	}

	impl Halves for Stuck {
		type Reader = Fed;
		type Writer = Waiting;

		fn halves(self) -> (Fed, Waiting) {
			(Fed(self.incoming), Waiting { written: 0 })
		}
	}

	#[tokio::test]
	async fn a_neighbor_that_takes_nothing_written_is_dropped_once_silent() {
		let relay = relay("rdu");
		let (feed, incoming) = mpsc::unbounded_channel();
		let hello = Message::Hello {
			version: VERSION,
			node: "tyo".into(),
			versions: Versions::new(),
			runs: None,
			minutes: None,
			beats: None,
			watch: None,
		};
		feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
		let silence = Duration::from_millis(300);
		let stuck = Heard::within(Stuck { incoming }, silence);
		let opened = Instant::now();
		let conversing = tokio::spawn(converse(relay.clone(), Some("tyo"), stuck));
		// This node's next snapshot, which the socket starts to write and never finishes.
		relay.take("rdu", Carried { version: 1, snapshot: Arc::new(serde_json::json!({})) }, None);
		let ended = tokio::time::timeout(Duration::from_secs(5), conversing)
			.await
			.expect("dropped, not left waiting on the write")
			.unwrap();
		// Whichever comes first: the reading silent, or the write waiting as long.
		let given_up = |error: &SocketError| error.silent() || error.stalled();
		assert!(matches!(ended, Err(MeshError::Socket(error)) if given_up(&error)));
		assert!(opened.elapsed() >= silence);
		drop(feed);
	}

	/// Smaller than one snapshot, so a write fills the other end's buffer long before it is read.
	const NARROW: u32 = 4 * 1024;

	/// Two ends of one socket on loopback, each with buffers of `NARROW`: the one dialed, and the one
	/// accepted, which takes the listener's.
	async fn narrow() -> (Dial, Dial) {
		let narrowed = || {
			let socket = tokio::net::TcpSocket::new_v4().unwrap();
			socket.set_send_buffer_size(NARROW).unwrap();
			socket.set_recv_buffer_size(NARROW).unwrap();
			socket
		};
		let listening = narrowed();
		listening.bind("127.0.0.1:0".parse().unwrap()).unwrap();
		let listener = listening.listen(1).unwrap();
		let address = listener.local_addr().unwrap();
		let (dialed, accepted) = tokio::join!(narrowed().connect(address), listener.accept());
		let (dialed, (accepted, _)) = (dialed.unwrap(), accepted.unwrap());
		let url = format!("ws://{address}{PATH}");
		let (client, server) = tokio::join!(
			tokio_tungstenite::client_async(url, MaybeTlsStream::Plain(dialed)),
			tokio_tungstenite::accept_async(MaybeTlsStream::Plain(accepted)),
		);
		(client.unwrap().0, server.unwrap())
	}

	/// What jammed `sin` and `nrt` on 2026-10-14: both ends writing faster than the other reads, so
	/// each waits on a write the other never reads, since it too is waiting on one. Each must still
	/// read, and so each still hears the other's newest once the writing stops.
	#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
	async fn two_ends_writing_faster_than_either_reads_keep_reading() {
		let (here, there) = (relay("rdu"), relay("sin"));
		let (dialed, accepted) = narrow().await;
		let silence = Duration::from_secs(10);
		let dialing = tokio::spawn(converse(here.clone(), Some("sin"), Heard::within(dialed, silence)));
		let accepting = tokio::spawn(converse(there.clone(), None, Heard::within(accepted, silence)));

		let snapshot = Arc::new(serde_json::json!({ "said": "x".repeat(1024 * 1024) }));
		let mut last = 0;
		let writing = Instant::now();
		while writing.elapsed() < Duration::from_secs(1) {
			last += 1;
			here.take("rdu", Carried { version: last, snapshot: snapshot.clone() }, None);
			there.take("sin", Carried { version: last, snapshot: snapshot.clone() }, None);
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
		let caught = async {
			while here.versions().get("sin") != Some(&last) || there.versions().get("rdu") != Some(&last)
			{
				tokio::time::sleep(Duration::from_millis(50)).await;
			}
		};
		let held = tokio::time::timeout(Duration::from_secs(20), caught).await;
		let (finished, ended) = (dialing.is_finished(), accepting.is_finished());
		dialing.abort();
		accepting.abort();
		assert!(held.is_ok(), "each holds the other's newest, {last}");
		assert!(!finished && !ended, "both sockets still open");
	}

	#[test]
	fn a_pong_carries_back_when_its_ping_was_sent() {
		let origin = Instant::now();
		let sent = origin + Duration::from_millis(40);
		let payload = stamp(origin, sent);
		let answered = sent + Duration::from_micros(151_250);
		assert_eq!(round_trip(origin, &payload, answered), Some(Duration::from_micros(151_250)));
		// One stamped later than now, and one this socket never sent, time nothing.
		assert_eq!(round_trip(origin, &payload, origin), None);
		assert_eq!(round_trip(origin, b"", answered), None);
		assert_eq!(round_trip(origin, b"four", answered), None);
	}

	#[test]
	fn the_secret_admits_and_nothing_else_does() {
		let with = |value: &str| {
			let mut headers = HeaderMap::new();
			headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
			headers
		};
		assert!(admitted(&with("Bearer s3cret"), "s3cret"));
		let wrongs =
			["Bearer s3cre", "Bearer s3cret!", "Bearer S3cret", "s3cret", "Basic s3cret", "Bearer "];
		for wrong in wrongs {
			assert!(!admitted(&with(wrong), "s3cret"), "{wrong}");
		}
		assert!(!admitted(&HeaderMap::new(), "s3cret"));
	}

	#[test]
	fn the_messages_are_tagged_by_type() {
		let versions = Versions::from([("tyo".into(), 7)]);
		let hello = Message::Hello {
			version: 1,
			node: "rdu".into(),
			versions,
			runs: None,
			minutes: None,
			beats: None,
			watch: None,
		};
		assert_eq!(
			serde_json::to_value(&hello).unwrap(),
			serde_json::json!({ "type": "hello", "version": 1, "node": "rdu", "versions": { "tyo": 7 } })
		);
		let node: Message = serde_json::from_str(
			r#"{ "type": "node", "node": "tyo", "state": { "version": 8, "snapshot": { "apps": [] } } }"#,
		)
		.unwrap();
		assert!(matches!(node, Message::Node { node, state } if node == "tyo" && state.version == 8));

		// A relay older than the runs says nothing of them, and is read as such.
		let older: Message =
			serde_json::from_str(r#"{ "type": "versions", "versions": { "tyo": 7 } }"#).unwrap();
		assert!(matches!(older, Message::Versions { runs: None, minutes: None, .. }));
		let runs: Message = serde_json::from_str(
			r#"{ "type": "runs", "node": "tyo", "above": 3, "version": 9, "rows": [{ "version": 9,
				"id": 1, "app": "geo", "action": "deploy", "source": { "kind": "panel" },
				"outcome": "running", "started_at": "2026-10-09T12:00:00Z" }] }"#,
		)
		.unwrap();
		assert!(matches!(runs, Message::Runs(Batch { node, above: 3, version: 9, rows })
			if node == "tyo" && rows[0].event.id == 1 && rows[0].version == 9));
	}
}
