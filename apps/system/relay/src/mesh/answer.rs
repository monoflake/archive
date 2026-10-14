//! What a neighbor lacks, queued for it: nodes, deltas, runs and minutes.

use super::outbox::Outbox;
use super::{MeshError, Message};
use crate::cluster::Versions;
use crate::delta::hex;
use crate::relay::Relay;
use std::collections::BTreeMap;

/// Queues for a synced `peer`, holding `theirs` -- each node's version, and its digest where it
/// said one -- what it lacks: a delta from its version where this relay still keeps that one, the
/// node whole otherwise, and whole again where the digests differ at the same version. Never
/// `peer`'s own, of which it is the source.
pub(super) fn answer(
	relay: &Relay,
	outbox: &mut Outbox,
	theirs: &BTreeMap<String, (u64, Option<String>)>,
	peer: &str,
	holds: &mut Versions,
) -> Result<(), MeshError> {
	for (node, (version, digest, _)) in relay.lines() {
		if node == peer {
			continue;
		}
		let message = match theirs.get(&node) {
			Some((held, _)) if *held > version => continue,
			Some((held, said)) if *held == version => {
				if said.as_ref().is_none_or(|said| *said == hex(digest)) {
					continue;
				}
				None
			}
			Some((held, _)) => relay.delta_from(&node, *held).and_then(|(to, digest, changes)| {
				(to == version).then(|| Message::Delta {
					node: node.clone(),
					base: *held,
					version,
					digest: hex(digest),
					changes,
				})
			}),
			None => None,
		};
		let message = match message {
			Some(delta) => delta,
			None => match relay.whole(&node) {
				Some(state) => Message::Node { node: node.clone(), state },
				None => continue,
			},
		};
		let own = (node == relay.node()).then_some(version);
		holds.insert(node, version);
		outbox.send(&message, own)?;
	}
	Ok(())
}

/// Queues for `peer`, holding `versions`, what it lacks.
pub(super) fn behind(
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
pub(super) async fn runs_behind(
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
pub(super) async fn minutes_behind(
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
