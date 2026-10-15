//! Every node's newest snapshot as this relay holds it, its newest beat, and the comparison two
//! relays make when they meet. A snapshot is carried as the JSON its node wrote, so a relay passes
//! on a field it is too old to know.

use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, VecDeque};
use std::sync::Arc;

/// How many of a node's newest versions are kept, for a delta to be taken from the one a neighbor
/// holds.
const KEPT: usize = 4;

/// One of a node's versions kept, with its digest.
#[derive(Debug, Clone)]
struct Past {
	version: u64,
	snapshot: Arc<Value>,
	digest: u64,
}

/// What a relay holds of one node.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Held {
	pub version: u64,
	/// When its origin last spoke -- its newest beat, or this version where that is newer -- never
	/// later than this relay's clock: a beat five minutes old is held as five minutes old, however it
	/// arrived. See spec/architecture/relay.md, "A node says it is leaving before it goes".
	pub heard_at: Timestamp,
	pub snapshot: Arc<Value>,
}

/// A node's snapshot as it travels between relays: when it was heard is each relay's own.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Carried {
	pub version: u64,
	pub snapshot: Arc<Value>,
}

/// Versions held, by node.
pub type Versions = BTreeMap<String, u64>;

#[derive(Debug, Default)]
pub struct Cluster {
	nodes: BTreeMap<String, Held>,
	/// Each node's newest beat, its origin's clock in milliseconds, held or not yet.
	beats: Versions,
	/// Each node's newest versions, the one held last.
	past: BTreeMap<String, VecDeque<Past>>,
}

impl Cluster {
	/// Takes `carried` when it is newer than what is held of `node`, or `node` is new; whether it
	/// did. An equal version is the same snapshot heard twice.
	pub fn merge(&mut self, node: &str, carried: Carried, now: Timestamp) -> bool {
		if self.nodes.get(node).is_some_and(|held| held.version >= carried.version) {
			return false;
		}
		let spoke = self.beats.get(node).map_or(carried.version, |beat| carried.version.max(*beat));
		let heard_at = heard(spoke, now);
		let digest = crate::delta::digest(&carried.snapshot);
		let past = self.past.entry(node.to_owned()).or_default();
		past.push_back(Past { version: carried.version, snapshot: carried.snapshot.clone(), digest });
		while past.len() > KEPT {
			past.pop_front();
		}
		let held = Held { version: carried.version, heard_at, snapshot: carried.snapshot };
		self.nodes.insert(node.to_owned(), held);
		true
	}

	/// The digest of the version held of `node`.
	pub fn digest(&self, node: &str) -> Option<u64> {
		self.past.get(node)?.back().map(|past| past.digest)
	}

	/// `node`'s snapshot at `version`, where it is one of those kept.
	pub fn at(&self, node: &str, version: u64) -> Option<Arc<Value>> {
		let past = self.past.get(node)?.iter().find(|past| past.version == version)?;
		Some(past.snapshot.clone())
	}

	/// Takes `node`'s beat at `at`, its origin's clock; when it was heard now, where that moved
	/// for a node held. An older beat than held moves nothing.
	pub fn beat(&mut self, node: &str, at: u64, now: Timestamp) -> Option<Timestamp> {
		if self.beats.get(node).is_some_and(|held| *held >= at) {
			return None;
		}
		self.beats.insert(node.to_owned(), at);
		let held = self.nodes.get_mut(node)?;
		let heard_at = heard(at.max(held.version), now);
		(heard_at > held.heard_at).then(|| {
			held.heard_at = heard_at;
			heard_at
		})
	}

	/// Each node's newest beat held, for a neighbor to take what it lacks.
	pub fn beats(&self) -> Versions {
		self.beats.clone()
	}

	pub fn versions(&self) -> Versions {
		self.nodes.iter().map(|(node, held)| (node.clone(), held.version)).collect()
	}

	/// What `peer`, holding `theirs`, lacks: every node held newer here, but never `peer`'s own,
	/// of which it is the source.
	pub fn lacking(&self, theirs: &Versions, peer: &str) -> Vec<(String, Carried)> {
		self
			.nodes
			.iter()
			.filter(|(node, held)| {
				*node != peer && theirs.get(*node).is_none_or(|version| *version < held.version)
			})
			.map(|(node, held)| {
				(node.clone(), Carried { version: held.version, snapshot: held.snapshot.clone() })
			})
			.collect()
	}

	pub fn nodes(&self) -> &BTreeMap<String, Held> {
		&self.nodes
	}
}

/// When a version was taken by its origin, whose clock in milliseconds it is, capped at `now`.
fn heard(version: u64, now: Timestamp) -> Timestamp {
	let taken = i64::try_from(version).ok().and_then(|ms| Timestamp::from_millisecond(ms).ok());
	taken.map_or(now, |taken| taken.min(now))
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn carried(version: u64, said: &str) -> Carried {
		Carried { version, snapshot: Arc::new(json!({ "said": said })) }
	}

	fn at(seconds: i64) -> Timestamp {
		Timestamp::from_second(1_790_000_000 + seconds).unwrap()
	}

	/// The version a node's clock gives at `seconds`.
	fn taken(seconds: i64) -> u64 {
		u64::try_from(at(seconds).as_millisecond()).unwrap()
	}

	#[test]
	fn a_newer_version_replaces_an_older_one() {
		let mut cluster = Cluster::default();
		assert!(cluster.merge("d62c82b23a858876", carried(taken(0), "old"), at(0)));
		assert!(cluster.merge("d62c82b23a858876", carried(taken(3), "new"), at(3)));
		let held = &cluster.nodes()["d62c82b23a858876"];
		assert_eq!((held.version, held.heard_at), (taken(3), at(3)));
		assert_eq!(held.snapshot["said"], "new");
	}

	#[test]
	fn a_version_is_heard_when_its_origin_took_it_never_later_than_now() {
		let mut cluster = Cluster::default();
		// Handed on start by a neighbor still holding it: five minutes old, not heard now.
		cluster.merge("d62c82b23a858876", carried(taken(0), "stale"), at(300));
		assert_eq!(cluster.nodes()["d62c82b23a858876"].heard_at, at(0));
		// From a clock ahead of this one, heard now rather than in the future.
		cluster.merge("d132b0f553e3dcc3", carried(taken(2), "ahead"), at(1));
		assert_eq!(cluster.nodes()["d132b0f553e3dcc3"].heard_at, at(1));
	}

	#[test]
	fn an_older_or_equal_version_is_ignored() {
		let mut cluster = Cluster::default();
		cluster.merge("d62c82b23a858876", carried(taken(0), "new"), at(0));
		assert!(!cluster.merge("d62c82b23a858876", carried(taken(0) - 1, "old"), at(3)));
		assert!(!cluster.merge("d62c82b23a858876", carried(taken(0), "again"), at(6)));
		let held = &cluster.nodes()["d62c82b23a858876"];
		assert_eq!((held.heard_at, held.snapshot["said"].as_str()), (at(0), Some("new")));
	}

	#[test]
	fn a_beat_moves_when_a_node_was_heard_and_never_back() {
		let mut cluster = Cluster::default();
		// A beat ahead of the snapshot it belongs to is kept until the snapshot comes.
		assert_eq!(cluster.beat("d62c82b23a858876", taken(2), at(2)), None);
		cluster.merge("d62c82b23a858876", carried(taken(0), "old"), at(2));
		assert_eq!(cluster.nodes()["d62c82b23a858876"].heard_at, at(2));
		assert_eq!(cluster.beat("d62c82b23a858876", taken(5), at(5)), Some(at(5)));
		assert_eq!(cluster.beat("d62c82b23a858876", taken(4), at(6)), None);
		assert_eq!(cluster.nodes()["d62c82b23a858876"].heard_at, at(5));
		assert_eq!(cluster.beats(), Versions::from([("d62c82b23a858876".into(), taken(5))]));
	}

	#[test]
	fn the_newest_versions_are_kept_with_their_digests() {
		let mut cluster = Cluster::default();
		for version in 1..=6 {
			cluster.merge("d62c82b23a858876", carried(version, &format!("v{version}")), at(0));
		}
		assert!(cluster.at("d62c82b23a858876", 2).is_none());
		assert_eq!(cluster.at("d62c82b23a858876", 3).unwrap()["said"], "v3");
		let newest = crate::delta::digest(&json!({ "said": "v6" }));
		assert_eq!(cluster.digest("d62c82b23a858876"), Some(newest));
		assert_eq!(cluster.digest("d132b0f553e3dcc3"), None);
	}

	#[test]
	fn an_unknown_node_is_added() {
		let mut cluster = Cluster::default();
		cluster.merge("d62c82b23a858876", carried(9, "d62c82b23a858876"), at(0));
		assert!(cluster.merge("d132b0f553e3dcc3", carried(1, "d132b0f553e3dcc3"), at(1)));
		assert_eq!(
			cluster.versions(),
			Versions::from([("d62c82b23a858876".into(), 9), ("d132b0f553e3dcc3".into(), 1)])
		);
	}

	#[test]
	fn meeting_sends_each_side_exactly_what_it_lacks() {
		let mut here = Cluster::default();
		let mut there = Cluster::default();
		for (node, version) in [
			("d62c82b23a858876", 9),
			("d132b0f553e3dcc3", 4),
			("e38f403f033d87f7", 7),
			("a0a24bbaa0c0beba", 2),
		] {
			here.merge(node, carried(version, "here"), at(0));
		}
		// `there` is tyo: its own is newer there, buf is equal, gvx older, rdu missing, nrt its own.
		for (node, version) in [
			("d132b0f553e3dcc3", 6),
			("e38f403f033d87f7", 7),
			("a0a24bbaa0c0beba", 1),
			("055d9b63bb419ba4", 3),
		] {
			there.merge(node, carried(version, "there"), at(0));
		}

		let to_there: Vec<(String, u64)> = here
			.lacking(&there.versions(), "d132b0f553e3dcc3")
			.into_iter()
			.map(|(node, carried)| (node, carried.version))
			.collect();
		assert_eq!(to_there, [("a0a24bbaa0c0beba".into(), 2), ("d62c82b23a858876".into(), 9)]);

		// tyo's own goes back however old it is here; nrt is new to `here`.
		let to_here: Vec<(String, u64)> = there
			.lacking(&here.versions(), "d62c82b23a858876")
			.into_iter()
			.map(|(node, carried)| (node, carried.version))
			.collect();
		assert_eq!(to_here, [("055d9b63bb419ba4".into(), 3), ("d132b0f553e3dcc3".into(), 6)]);
	}
}
