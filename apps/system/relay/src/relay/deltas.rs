//! What a sync carries and a delta takes: each node's line, whole, or changed from a version.

use super::{Applied, Relay, lock};
use crate::cluster::Carried;
use crate::delta::Changes;
use std::collections::BTreeMap;
use std::sync::Arc;

impl Relay {
	/// Each node held, with its version, the digest of it, and its newest beat: what a sync says.
	pub fn lines(&self) -> BTreeMap<String, (u64, u64, u64)> {
		let holding = lock(&self.holding);
		let (cluster, beats) = (&holding.cluster, holding.cluster.beats());
		let digest = |node: &str| cluster.digest(node).unwrap_or_default();
		let beat = |node: &str| beats.get(node).copied().unwrap_or_default();
		let nodes = cluster.nodes().iter();
		nodes.map(|(node, held)| (node.clone(), (held.version, digest(node), beat(node)))).collect()
	}

	/// `node` as held, whole.
	pub fn whole(&self, node: &str) -> Option<Carried> {
		let holding = lock(&self.holding);
		let held = holding.cluster.nodes().get(node)?;
		Some(Carried { version: held.version, snapshot: held.snapshot.clone() })
	}

	/// What takes `node` from `base` to the version held: that version, its digest, and the
	/// changes; none where `base` is no longer kept.
	pub fn delta_from(&self, node: &str, base: u64) -> Option<(u64, u64, Changes)> {
		let holding = lock(&self.holding);
		let (cluster, held) = (&holding.cluster, holding.cluster.nodes().get(node)?);
		let from = cluster.at(node, base)?;
		Some((held.version, cluster.digest(node)?, crate::delta::diff(&from, &held.snapshot)))
	}

	/// A neighbor's delta of `node`, from `base` to `version`, taken where this relay holds `base`
	/// and the result reads `digest`. A neighbor's word on this node is never taken.
	pub fn take_delta(
		&self,
		node: &str,
		(base, version): (u64, u64),
		digest: &str,
		changes: &Changes,
		via: &str,
	) -> Applied {
		if node == self.node {
			return Applied::Held;
		}
		let held = {
			let holding = lock(&self.holding);
			holding.cluster.nodes().get(node).map(|held| (held.version, held.snapshot.clone()))
		};
		let snapshot = match held {
			Some((held, _)) if held >= version => return Applied::Held,
			Some((held, snapshot)) if held == base => snapshot,
			_ => return Applied::Wanted,
		};
		let Some(next) = crate::delta::apply(&snapshot, changes) else {
			return Applied::Wanted;
		};
		if crate::delta::hex(crate::delta::digest(&next)) != digest {
			return Applied::Wanted;
		}
		self.take(node, Carried { version, snapshot: Arc::new(next) }, Some(via));
		Applied::Taken
	}
}
