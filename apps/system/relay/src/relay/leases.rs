//! The leases on a node's fine readings, and the readings lent under them.

use super::{LEASE, Pulse, Relay, lock};
use serde_json::Value;
use std::time::{Duration, Instant};

impl Relay {
	/// A page here asks for `node`'s fine readings: this node's own lent at once, any other's asked
	/// of its origin over the mesh, for `LEASE`.
	pub fn watch(&self, node: &str) {
		if node == self.node {
			lock(&self.leases).insert(self.node.clone(), Instant::now() + LEASE);
		} else {
			let _ = self.asking.send(node.to_owned());
		}
	}

	/// `peer` holds a lease on this node's fine readings for `lease`, at most `LEASE`.
	pub fn leased(&self, peer: &str, lease: Duration) {
		lock(&self.leases).insert(peer.to_owned(), Instant::now() + lease.min(LEASE));
	}

	/// Whether `peer`, or a page here by this node's name, holds a lease still.
	pub fn leased_to(&self, peer: &str) -> bool {
		lock(&self.leases).get(peer).is_some_and(|until| *until > Instant::now())
	}

	/// This node's fine readings of the round, to a page here and to every neighbor with a lease.
	pub(super) fn lend(&self, sample: Value) {
		let neighbors = {
			let now = Instant::now();
			let mut leases = lock(&self.leases);
			leases.retain(|_, until| *until > now);
			leases.keys().any(|holder| *holder != self.node)
		};
		if self.leased_to(&self.node) {
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
