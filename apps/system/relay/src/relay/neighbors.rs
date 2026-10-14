//! The neighbors a relay holds: retired and named peers, conversations, and round trips.

use super::{CURRENT, Change, GRACE, Holding, Relay, lock};
use crate::cluster::Versions;
use crate::own::RoundTrips;
use jiff::Timestamp;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

impl Relay {
	/// Whether a neighbor older than the beat is met, which hears a node by its versions alone: so
	/// long as one is, this node's own is given a new version every round.
	pub(super) fn every_round(&self) -> bool {
		lock(&self.met).values().any(|beats| !beats)
	}

	/// A conversation with a neighbor introduced, and whether that neighbor hears by beats.
	pub fn met(&self, conversation: u64, beats: bool) {
		lock(&self.met).insert(conversation, beats);
	}

	/// A conversation ended.
	pub fn parted(&self, conversation: u64) {
		lock(&self.met).remove(&conversation);
	}

	/// Whether this relay reads and sends compressed frames; see `compression`.
	pub fn compresses(&self) -> bool {
		self.compressing.load(Ordering::Relaxed)
	}

	/// Turns compressed frames off from the next conversation on: a relay that stands in for one
	/// older than them, as a test does.
	pub fn set_compression(&self, on: bool) {
		self.compressing.store(on, Ordering::Relaxed);
	}

	/// Whether this relay syncs by digest; see `mesh::Message::Sync`.
	pub fn syncs(&self) -> bool {
		self.syncing.load(Ordering::Relaxed)
	}

	/// Turns the sync off from the next conversation on, as `set_compression` turns compression.
	pub fn set_sync(&self, on: bool) {
		self.syncing.store(on, Ordering::Relaxed);
	}

	/// The nodes held as retired from now on, every one named.
	pub fn set_retired(&self, retired: BTreeSet<String>) {
		*lock(&self.retired) = retired;
	}

	/// Whether `node` is held as retired, so its relay is not spoken with.
	pub fn is_retired(&self, node: &str) -> bool {
		lock(&self.retired).contains(node)
	}

	/// The neighbors dialed from now on. A new one is shown waiting at once; one no longer dialed
	/// stops being shown waiting, and what is held of it ages as any silent node's does.
	pub fn set_peers(&self, peers: BTreeSet<String>) {
		{
			let mut holding = lock(&self.holding);
			let Holding { cluster, shown, peers: held, .. } = &mut *holding;
			for gone in held.difference(&peers) {
				if !cluster.nodes().contains_key(gone) {
					shown.remove(gone);
				}
			}
			*held = peers;
		}
		self.sweep();
	}

	/// Every node whose state time alone has changed since it was last shown, sent again.
	pub fn sweep(&self) {
		self.swept(Timestamp::now(), self.started.elapsed());
	}

	pub(super) fn swept(&self, now: Timestamp, uptime: Duration) {
		let timed = self.round_trips();
		let mut holding = lock(&self.holding);
		for (node, entry) in holding.entries(now, uptime, (self.node.as_str(), &timed)) {
			if holding.shown.get(&node) != Some(&entry.state) {
				holding.shown.insert(node.clone(), entry.state);
				let _ = self.changes.send(Change { node, entry });
			}
		}
	}

	/// A number for a socket to a neighbor, its own among them.
	pub fn conversation(&self) -> u64 {
		self.conversations.fetch_add(1, Ordering::Relaxed)
	}

	/// Whether `conversation` is the socket to `peer` that sends unasked: the first to ask is, until
	/// it is hushed.
	pub fn speaks(&self, peer: &str, conversation: u64) -> bool {
		*lock(&self.speakers).entry(peer.to_owned()).or_insert(conversation) == conversation
	}

	pub fn hush(&self, peer: &str, conversation: u64) {
		let mut speakers = lock(&self.speakers);
		if speakers.get(peer) == Some(&conversation) {
			speakers.remove(peer);
		}
	}

	/// The neighbors a socket is open to and speaking.
	pub fn speaking(&self) -> BTreeSet<String> {
		lock(&self.speakers).keys().cloned().collect()
	}

	/// This node's own `version` sent to `peer`.
	pub fn told(&self, peer: &str, version: u64) {
		self.told.send_modify(|told| {
			let sent = told.entry(peer.to_owned()).or_default();
			*sent = (*sent).max(version);
		});
	}

	/// Once every one of `neighbors` has been sent this node's own `version` or a newer one.
	pub async fn told_of(&self, neighbors: &BTreeSet<String>, version: u64) {
		let mut told = self.told.subscribe();
		let every =
			|told: &Versions| neighbors.iter().all(|peer| told.get(peer).is_some_and(|v| *v >= version));
		// The sender lives as long as the relay, which outlives this wait.
		let _ = told.wait_for(every).await;
	}

	/// A round trip to `peer` just timed, on either socket to it; the latest replaces the last.
	pub fn timed(&self, peer: &str, round_trip: Duration) {
		lock(&self.timed).insert(peer.to_owned(), (round_trip, Instant::now()));
	}

	/// The kernel's round trip to `peer` just read, on the socket dialed to it.
	pub fn measured(&self, peer: &str, round_trip: Duration) {
		lock(&self.measured).insert(peer.to_owned(), (round_trip, Instant::now()));
	}

	/// Each neighbor's latest round trip, the kernel's over the pings', a neighbor not timed lately
	/// left out.
	pub fn round_trips(&self) -> RoundTrips {
		let now = Instant::now();
		let mut round_trips = current(&lock(&self.timed), now);
		round_trips.extend(current(&lock(&self.measured), now));
		round_trips
	}

	/// Once host has been read, or the grace is over.
	pub fn healthy(&self) -> bool {
		lock(&self.own).read() || self.started.elapsed() >= GRACE
	}
}

/// The round trips timed within `CURRENT` of `now`, in seconds to a ten-thousandth -- a tenth of a
/// millisecond, since a neighbor in the same place answers in under one; workspace spec/json.md,
/// "Keys are stable; values are cheap", has durations in seconds.
pub(super) fn current(timed: &BTreeMap<String, (Duration, Instant)>, now: Instant) -> RoundTrips {
	timed
		.iter()
		.filter(|(_, (_, at))| now.saturating_duration_since(*at) < CURRENT)
		.map(|(peer, (round_trip, _))| {
			(peer.clone(), (round_trip.as_secs_f64() * 10_000.0).round() / 10_000.0)
		})
		.collect()
}
