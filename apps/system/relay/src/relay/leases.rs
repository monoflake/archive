//! The pages here watching every node, the leases neighbors hold on this node's fine readings, and
//! the readings lent under them.

use super::{LEASE, Pulse, Relay, lock};
use serde_json::Value;
use std::sync::Arc;
use std::time::{Duration, Instant};

/// How many pages here watch every node, and when every neighbor was last asked to lend.
#[derive(Debug, Default)]
pub(super) struct Watchers {
	pages: usize,
	asked: Option<Instant>,
}

/// A page's watch on every node's fine readings, held for as long as its stream is open: the first
/// asks every neighbor that lends for `LEASE`, the round asks again before it lapses, and the last
/// to go releases them. See spec/architecture/relay.md, RFC grmtb.
pub struct Watching(Arc<Relay>);

impl Drop for Watching {
	fn drop(&mut self) {
		let mut watchers = lock(&self.0.watchers);
		watchers.pages -= 1;
		if watchers.pages == 0 {
			watchers.asked = None;
			let _ = self.0.asking.send(Duration::ZERO);
		}
	}
}

impl Relay {
	/// A page here watches every node until the returned watch is dropped.
	pub fn watch(self: &Arc<Self>) -> Watching {
		let mut watchers = lock(&self.watchers);
		watchers.pages += 1;
		self.ask(&mut watchers, Instant::now());
		Watching(self.clone())
	}

	/// Whether a page here watches every node.
	pub fn watched(&self) -> bool {
		lock(&self.watchers).pages > 0
	}

	/// Every neighbor asked again at `now`, where a page watches and half a lease has passed since
	/// the last ask, so one ask lost does not let a lease lapse.
	pub(super) fn renew(&self, now: Instant) {
		self.ask(&mut lock(&self.watchers), now);
	}

	fn ask(&self, watchers: &mut Watchers, now: Instant) {
		let due = watchers.asked.is_none_or(|asked| now >= asked + LEASE / 2);
		if watchers.pages > 0 && due {
			watchers.asked = Some(now);
			let _ = self.asking.send(LEASE);
		}
	}

	/// `peer` holds a lease on this node's fine readings for `lease`, at most `LEASE`; none releases
	/// it.
	pub fn leased(&self, peer: &str, lease: Duration) {
		lock(&self.leases).insert(peer.to_owned(), Instant::now() + lease.min(LEASE));
	}

	/// Whether `peer` holds a lease still.
	pub fn leased_to(&self, peer: &str) -> bool {
		lock(&self.leases).get(peer).is_some_and(|until| *until > Instant::now())
	}

	/// This node's fine readings of the round, to a page here and to every neighbor with a lease.
	pub(super) fn lend(&self, sample: Value) {
		let neighbors = {
			let now = Instant::now();
			let mut leases = lock(&self.leases);
			leases.retain(|_, until| *until > now);
			!leases.is_empty()
		};
		if self.watched() {
			let node = self.node.clone();
			let _ = self.pulses.send(Pulse::Readings { node, sample: sample.clone() });
		}
		if neighbors {
			let _ = self.lent.send(sample);
		}
	}

	/// A node's fine readings, from its origin, for a page here.
	pub fn heard_readings(&self, node: &str, sample: Value) {
		let _ = self.pulses.send(Pulse::Readings { node: node.to_owned(), sample });
	}
}
