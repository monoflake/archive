//! One socket to a neighbor, from the introductions until it closes, and its pings timed.

use super::answer::{answer, behind, minutes_behind, runs_behind};
use super::outbox::{Outbox, QUEUE, Writing, write, written};
use super::{COMPARISON, INTRODUCTION, Line, MeshError, Message, PING, SYNC, Said, introduced};
use crate::cluster::{Carried, Versions};
use crate::compression::{Packer, Unpacker};
use crate::delta::{DIGEST, hex};
use crate::relay::{Applied, Relay, Update, VERSION};
use crate::socket::{Frame, Halves, Heard, Reader, Received, SocketError, Writer};
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast::error::RecvError;
use tokio::sync::mpsc;
use tokio::time::Instant;

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
		zstd: relay.compresses().then_some(true),
		sync: relay.syncs().then(|| DIGEST.to_owned()),
	};
	writer.send(Frame::Text(serde_json::to_string(&hello)?)).await?;

	let theirs = tokio::time::timeout(INTRODUCTION, introduced(&mut reader))
		.await
		.map_err(|_| MeshError::Slow(INTRODUCTION))??;
	let said = match theirs {
		Message::Hello { version, .. } if version != VERSION => {
			return Err(MeshError::Version(version));
		}
		Message::Hello { node, versions, runs, minutes, beats, watch, zstd, sync, .. } => Said {
			peer: node,
			versions,
			runs,
			minutes,
			beats: beats == Some(true),
			watch: watch == Some(true),
			zstd: zstd == Some(true) && relay.compresses(),
			sync: sync.as_deref() == Some(DIGEST) && relay.syncs(),
		},
		_ => return Err(MeshError::Unintroduced),
	};
	let Said { peer, versions, runs, minutes: theirs_minutes, .. } = said;
	let (hears_beats, lends, packs, synced) = (said.beats, said.watch, said.zstd, said.sync);
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
	// What this end sends is compressed for a neighbor that reads it; what it receives compressed,
	// it unpacks, on a stream of its own begun with the first such frame.
	let packer = if packs { Some(Packer::new().map_err(SocketError::from)?) } else { None };
	let mut unpacker: Option<Unpacker> = None;
	let writes = write(relay.clone(), peer.clone(), writer, packer, queued);
	let mut writing = Writing(tokio::spawn(writes));
	// A neighbor that said nothing of runs or minutes is older than them; see `Message`.
	let (reads_runs, reads_minutes) = (runs.is_some(), theirs_minutes.is_some());
	// The version of each node a synced neighbor holds, as it said or as it was sent.
	let mut holds = if synced { versions.clone() } else { Versions::new() };
	if synced {
		let theirs = versions.iter().map(|(node, version)| (node.clone(), (*version, None))).collect();
		answer(&relay, &mut outbox, &theirs, &peer, &mut holds)?;
	} else {
		behind(&relay, &mut outbox, &versions, &peer)?;
	}
	if let Some(runs) = runs {
		runs_behind(&relay, &mut outbox, &runs, &peer).await?;
	}
	if let Some(theirs) = theirs_minutes {
		minutes_behind(&relay, &mut outbox, &theirs, &peer).await?;
	}

	let every = if synced { SYNC } else { COMPARISON };
	let mut comparison = tokio::time::interval_at(Instant::now() + every, every);
	let mut ping = tokio::time::interval(PING);
	let origin = Instant::now();
	loop {
		tokio::select! {
			received = reader.receive() => {
				let text = match received? {
					Received::Text(text) => Some(text.into_bytes()),
					// A relay with them off reads none, as one older than them passes them over.
					Received::Binary(_) if !relay.compresses() => None,
					Received::Binary(packed) => {
						let unpacker = match &mut unpacker {
							Some(unpacker) => unpacker,
							None => unpacker.insert(Unpacker::new().map_err(MeshError::Corrupt)?),
						};
						Some(unpacker.unpack(&packed).map_err(MeshError::Corrupt)?)
					}
					Received::Pong(payload) => {
						if let Some(round_trip) = round_trip(origin, &payload, Instant::now()) {
							relay.timed(&peer, round_trip);
						}
						None
					}
					Received::Closed => break,
					Received::Other => None,
				};
				if let Some(text) = text {
					match serde_json::from_slice(&text)? {
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
						Message::Sync { nodes, runs, minutes } => {
							let mut theirs = BTreeMap::new();
							for (node, Line(version, digest, beat)) in nodes {
								relay.beaten(&node, beat, Some(&peer));
								holds.insert(node.clone(), version);
								theirs.insert(node, (version, Some(digest)));
							}
							answer(&relay, &mut outbox, &theirs, &peer, &mut holds)?;
							runs_behind(&relay, &mut outbox, &runs, &peer).await?;
							minutes_behind(&relay, &mut outbox, &minutes, &peer).await?;
						}
						Message::Delta { node, base, version, digest, changes } => {
							let taken = relay.take_delta(&node, (base, version), &digest, &changes, &peer);
							if taken == Applied::Wanted {
								outbox.send(&Message::Want { node }, None)?;
							}
						}
						Message::Want { node } => {
							if let Some(state) = relay.whole(&node) {
								holds.insert(node.clone(), state.version);
								let own = (node == relay.node()).then_some(state.version);
								outbox.send(&Message::Node { node, state }, own)?;
							}
						}
						Message::Hello { .. } => {}
					}
				}
			}
			update = updates.recv() => match update {
				Ok(Update { node, held }) => {
					if node == relay.node() && speaking.speaks() {
						let version = held.version;
						let delta = holds.get(&node).filter(|_| synced).and_then(|base| {
							let (to, digest, changes) = relay.delta_from(&node, *base)?;
							(to == version).then(|| Message::Delta {
								node: node.clone(),
								base: *base,
								version,
								digest: hex(digest),
								changes,
							})
						});
						let message = delta.unwrap_or_else(|| {
							let state = Carried { version, snapshot: held.snapshot };
							Message::Node { node: node.clone(), state }
						});
						holds.insert(node, version);
						outbox.send(&message, Some(version))?;
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
				// A synced neighbor has this node's beat in every sync.
				Ok(at) => {
					if hears_beats && !synced && speaking.speaks() {
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
				if speaking.speaks() && synced {
					let lines = relay.lines().into_iter();
					let nodes = lines.map(|(node, (version, digest, beat))| (node, Line(version, hex(digest), beat)));
					let (runs, minutes) = (relay.run_versions(), relay.minute_versions());
					outbox.send(&Message::Sync { nodes: nodes.collect(), runs, minutes }, None)?;
				} else if speaking.speaks() {
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
pub(super) fn stamp(origin: Instant, now: Instant) -> Bytes {
	let sent = u64::try_from(now.duration_since(origin).as_micros()).unwrap_or(u64::MAX);
	Bytes::copy_from_slice(&sent.to_be_bytes())
}

/// The round trip a pong closes, or none for a pong to no ping of this socket's: another shape, or
/// sent later than `now`.
pub(super) fn round_trip(origin: Instant, payload: &[u8], now: Instant) -> Option<Duration> {
	let sent = u64::from_be_bytes(payload.try_into().ok()?);
	let elapsed = now.duration_since(origin);
	elapsed.checked_sub(Duration::from_micros(sent))
}
