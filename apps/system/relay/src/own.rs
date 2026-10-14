//! This node's own snapshot, of which this relay is the one source: read from host every round,
//! and given a new version only when what it says changes. Being heard is the beat's; the machine's
//! readings stay on the node, a minute of them on the mesh; the events are read into the runs and
//! never carried here -- RFC grmtb.

use crate::alarm::{Alarms, Judge};
use crate::history::Readings;
use crate::host::{App, ReadError, Reading};
use crate::presence::Leaving;
use crate::window::version_after;
use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

/// How often host is read.
pub const EVERY: std::time::Duration = std::time::Duration::from_secs(3);

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Part {
	Events,
	Apps,
	Machine,
}

impl std::fmt::Display for Part {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str(match self {
			Self::Events => "events",
			Self::Apps => "apps",
			Self::Machine => "machine",
		})
	}
}

/// What a snapshot says: only what changes on purpose, so a version moves when it does.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct Content {
	pub apps: Vec<App>,
	/// The meter's `info`, as `{ "info": ... }`: what does not change while the machine is up.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub machine: Option<Value>,
	/// The parts the last round failed to read, each held at what was read before.
	#[serde(skip_serializing_if = "BTreeSet::is_empty")]
	pub stale: BTreeSet<Part>,
	/// Said once the relay is stopping, and in every snapshot after: a newer one without it is the
	/// node back.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub leaving: Option<Leaving>,
	/// The thresholds that hold, each since when; see `alarm`.
	#[serde(skip_serializing_if = "BTreeMap::is_empty")]
	pub alarms: Alarms,
}

#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Snapshot {
	/// When this version was taken.
	pub taken_at: Timestamp,
	#[serde(flatten)]
	pub content: Content,
}

/// This relay's round trip to each neighbor, in seconds, by neighbor.
pub type RoundTrips = BTreeMap<String, f64>;

/// The parts a round failed to read, each with why.
pub type Failures = Vec<(Part, ReadError)>;

#[derive(Debug, Default)]
pub struct Own {
	content: Content,
	version: u64,
	/// Whether events and apps have both been read in one round.
	read: bool,
	/// The meter's latest second, for a page watching this node; never part of a version.
	sample: Option<Value>,
	judge: Judge,
}

impl Own {
	/// One round taken in, with the meter's minute where one just ended: a new version and its
	/// snapshot where what it says changed, or where `every_round` asks for one regardless -- a
	/// neighbor older than the beat hears a node by its versions. A failure is given back beside
	/// it with its part.
	pub fn observe(
		&mut self,
		reading: Reading,
		minute: Option<&(i64, Readings)>,
		every_round: bool,
		now: Timestamp,
	) -> (Option<(u64, Snapshot)>, Failures) {
		let Reading { events, apps, machine } = reading;
		let mut next = self.content.clone();
		let mut failed = Vec::new();
		if let Err(error) = events {
			failed.push((Part::Events, error));
		}
		kept(Part::Apps, apps, &mut next.apps, &mut failed);
		match machine {
			Ok(machine) => {
				self.sample = machine.get("sample").cloned();
				let info = machine.get("info").cloned();
				next.machine = info.map(|info| serde_json::json!({ "info": info })).or(next.machine);
			}
			Err(error) => failed.push((Part::Machine, error)),
		}
		next.stale = failed.iter().map(|(part, _)| *part).collect();
		if let Some((at, readings)) = minute {
			let info = next.machine.as_ref().and_then(|machine| machine.get("info"));
			if self.judge.minute(*at, readings, info) {
				next.alarms = self.judge.said().clone();
			}
		}
		self.read |= !next.stale.contains(&Part::Events) && !next.stale.contains(&Part::Apps);
		let changed = next != self.content || self.version == 0;
		self.content = next;
		((changed || every_round).then(|| self.versioned(now)), failed)
	}

	/// The last snapshot, saying the node is leaving; the rounds after it say so too.
	pub fn leave(&mut self, leaving: Leaving, now: Timestamp) -> (u64, Snapshot) {
		self.content.leaving = Some(leaving);
		self.versioned(now)
	}

	fn versioned(&mut self, now: Timestamp) -> (u64, Snapshot) {
		self.version = version_after(self.version, now);
		(self.version, Snapshot { taken_at: now, content: self.content.clone() })
	}

	pub fn read(&self) -> bool {
		self.read
	}

	pub fn stale(&self) -> &BTreeSet<Part> {
		&self.content.stale
	}

	/// The meter's latest second, as the last round read it.
	pub fn sample(&self) -> Option<&Value> {
		self.sample.as_ref()
	}
}

/// `read` into `into`, or `into` left as it was and the failure noted.
fn kept<T>(part: Part, read: Result<T, ReadError>, into: &mut T, failed: &mut Failures) {
	match read {
		Ok(value) => *into = value,
		Err(error) => failed.push((part, error)),
	}
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::history::Gauge;
	use serde_json::json;

	fn app(image: &str) -> App {
		App {
			name: "geo".into(),
			image: image.into(),
			deployed_at: "2026-10-05T08:00:00Z".into(),
			running: true,
			held: false,
			rollout: "replace".into(),
			label: None,
			driver: false,
		}
	}

	/// A round reading `image`, the meter's second at `cpu`.
	fn read(image: &str, cpu: f64) -> Reading {
		let machine =
			json!({ "info": { "memory": 1000 }, "sample": { "values": { "cpu.usage": cpu } } });
		Reading { events: Ok(vec![]), apps: Ok(vec![app(image)]), machine: Ok(machine) }
	}

	fn reading(image: &str) -> Reading {
		read(image, 1.0)
	}

	fn at(milliseconds: i64) -> Timestamp {
		Timestamp::from_millisecond(1_790_000_000_000 + milliseconds).unwrap()
	}

	/// The version and snapshot a round gives, which must be one.
	fn versioned(own: &mut Own, reading: Reading, now: Timestamp) -> (u64, Snapshot) {
		own.observe(reading, None, false, now).0.expect("a new version")
	}

	#[test]
	fn a_version_moves_only_when_what_it_says_does() {
		let mut own = Own::default();
		let (first, snapshot) = versioned(&mut own, read("geo:1", 3.0), at(0));
		assert_eq!((first, snapshot.taken_at), (1_790_000_000_000, at(0)));
		// The meter's second moved, and nothing the snapshot says did.
		assert!(own.observe(read("geo:1", 70.0), None, false, at(3000)).0.is_none());
		assert_eq!(own.sample(), Some(&json!({ "values": { "cpu.usage": 70.0 } })));
		let (second, snapshot) = versioned(&mut own, reading("geo:2"), at(6000));
		assert_eq!(second, first + 6000);
		assert_eq!(snapshot.content.apps[0].image, "geo:2");
		let written = serde_json::to_value(&snapshot).unwrap();
		assert_eq!(written["machine"], json!({ "info": { "memory": 1000 } }));
		assert!(written.get("round_trip").is_none() && written.get("events").is_none());
	}

	#[test]
	fn a_neighbor_older_than_the_beat_is_given_a_version_every_round() {
		let mut own = Own::default();
		let (first, _) = versioned(&mut own, reading("geo:1"), at(0));
		let (again, _) = own.observe(reading("geo:1"), None, true, at(3000)).0.unwrap();
		assert_eq!(again, first + 3000);
	}

	#[test]
	fn leaving_is_said_in_the_last_snapshot_and_every_round_after() {
		let mut own = Own::default();
		own.observe(reading("geo:1"), None, false, at(0));
		let leaving = Leaving { reason: crate::presence::Reason::Upgrade, within: 180 };
		let (version, snapshot) = own.leave(leaving, at(1000));
		assert_eq!(version, 1_790_000_001_000);
		let written = serde_json::to_value(&snapshot).unwrap();
		assert_eq!(written["leaving"], json!({ "reason": "upgrade", "within": 180 }));
		// A round already under way as the relay stops does not say the node is back.
		let (after, _) = own.observe(reading("geo:1"), None, true, at(1000));
		assert_eq!(after.unwrap().1.content.leaving, Some(leaving));
	}

	#[test]
	fn a_threshold_held_is_said_in_a_new_version_and_its_clearing_in_another() {
		let mut own = Own::default();
		own.observe(reading("geo:1"), None, false, at(0));
		let minute = |n: i64, cpu: f64| {
			let readings = Readings::from([("cpu.usage".into(), Gauge { mean: cpu, max: cpu })]);
			(n * 60_000, readings)
		};
		for n in 0..4 {
			assert!(
				own.observe(reading("geo:1"), Some(&minute(n, 95.0)), false, at(n * 60_000)).0.is_none()
			);
		}
		let (_, said) =
			own.observe(reading("geo:1"), Some(&minute(4, 95.0)), false, at(240_000)).0.unwrap();
		assert!(said.content.alarms.contains_key("cpu"));
		let (_, cleared) =
			own.observe(reading("geo:1"), Some(&minute(5, 10.0)), false, at(300_000)).0.unwrap();
		assert!(cleared.content.alarms.is_empty());
	}

	#[test]
	fn a_snapshot_carries_no_events_and_a_failed_read_of_them_is_stale() {
		let mut own = Own::default();
		own.observe(reading("geo:1"), None, false, at(0));
		let failing = Reading {
			events: Err(ReadError::Slow(std::time::Duration::from_secs(5))),
			apps: Ok(vec![app("geo:1")]),
			machine: Ok(json!({ "info": { "memory": 1000 } })),
		};
		let (snapshot, failed) = own.observe(failing, None, false, at(3000));
		let stale = snapshot.unwrap().1.content.stale;
		assert_eq!((stale, failed.len()), (BTreeSet::from([Part::Events]), 1));
	}

	#[test]
	fn a_version_climbs_when_the_clock_does_not() {
		assert_eq!(version_after(1_790_000_000_500, at(0)), 1_790_000_000_501);
		// Restarted: the last is forgotten, and the clock alone puts it above what it gave out.
		assert_eq!(version_after(0, at(0)), 1_790_000_000_000);
	}

	#[test]
	fn a_failed_read_keeps_what_was_read_and_says_it_is_stale() {
		let mut own = Own::default();
		own.observe(reading("geo:1"), None, false, at(0));
		let failing = Reading {
			events: Ok(vec![]),
			apps: Err(ReadError::Slow(std::time::Duration::from_secs(5))),
			machine: Err(ReadError::Slow(std::time::Duration::from_secs(5))),
		};
		let (snapshot, failed) = own.observe(failing, None, false, at(3000));
		let snapshot = snapshot.unwrap().1;
		assert_eq!(snapshot.content.apps, [app("geo:1")]);
		assert_eq!(snapshot.content.stale, BTreeSet::from([Part::Apps, Part::Machine]));
		assert_eq!(failed.len(), 2);
		let written = serde_json::to_value(&snapshot).unwrap();
		assert_eq!(written["stale"], json!(["apps", "machine"]));
		assert_eq!(written["machine"], json!({ "info": { "memory": 1000 } }));
		assert!(written.get("taken_at").is_some());
	}

	#[test]
	fn host_counts_as_read_once_events_and_apps_both_are() {
		let mut own = Own::default();
		fn unread<T>() -> Result<T, ReadError> {
			Err(ReadError::Slow(std::time::Duration::from_secs(5)))
		}
		let neither = Reading { events: unread(), apps: Ok(vec![]), machine: unread() };
		own.observe(neither, None, false, at(0));
		assert!(!own.read());
		let both = Reading { events: Ok(vec![]), apps: Ok(vec![]), machine: unread() };
		own.observe(both, None, false, at(3000));
		assert!(own.read());
	}
}
