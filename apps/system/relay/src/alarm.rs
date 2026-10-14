//! Thresholds judged on the node, from the meter's minute: a reading high for long enough is said
//! in the node's own snapshot, a new version sent at once, so an alarm needs nobody watching; it
//! clears with the first minute below a lower line, so a reading at the line does not set and clear
//! it every minute. See spec/architecture/relay.md, RFC grmtb.

use crate::history::{MINUTE, Readings};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;
use std::collections::BTreeMap;

/// A reading judged against a line: its minute mean, or that mean as a share of a total the
/// machine's info holds.
pub struct Rule {
	/// What the alarm is called in the snapshot; a rule over every zone of a prefix is called by
	/// each zone's metric.
	pub alarm: &'static str,
	/// The meter's metric, or a dotted prefix whose every metric is judged apart.
	pub metric: &'static str,
	/// The total in the meter's info the mean is a percent of, where it is one.
	pub of: Option<&'static str>,
	pub at_least: f64,
	/// The minutes in a row it must hold.
	pub minutes: u32,
	/// The line a minute must fall below to clear it once said.
	pub clears_below: f64,
}

/// Decided with the author on 2026-10-14.
pub const RULES: [Rule; 4] = [
	Rule {
		alarm: "cpu",
		metric: "cpu.usage",
		of: None,
		at_least: 90.0,
		minutes: 5,
		clears_below: 85.0,
	},
	Rule {
		alarm: "memory",
		metric: "memory.used",
		of: Some("memory"),
		at_least: 90.0,
		minutes: 5,
		clears_below: 85.0,
	},
	Rule {
		alarm: "storage",
		metric: "storage.used",
		of: Some("storage"),
		at_least: 90.0,
		minutes: 1,
		clears_below: 88.0,
	},
	Rule {
		alarm: "temperature",
		metric: "temperature",
		of: None,
		at_least: 85.0,
		minutes: 5,
		clears_below: 80.0,
	},
];

/// An alarm said: since when its condition has held.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct Alarm {
	pub since: Timestamp,
}

/// What is said, by alarm.
pub type Alarms = BTreeMap<String, Alarm>;

/// Each alarm's minutes held in a row, and what is said.
#[derive(Debug, Default)]
pub struct Judge {
	held: BTreeMap<String, u32>,
	said: Alarms,
}

impl Judge {
	/// The minute starting `at`, in milliseconds, judged; whether what is said changed. A metric
	/// the minute does not carry leaves its alarm as it was: no reading is not a low one.
	pub fn minute(&mut self, at: i64, readings: &Readings, info: Option<&Value>) -> bool {
		let before = self.said.clone();
		for rule in &RULES {
			for (alarm, value) in judged(rule, readings, info) {
				if value < rule.clears_below {
					self.said.remove(&alarm);
				}
				if value < rule.at_least {
					self.held.remove(&alarm);
					continue;
				}
				let held = self.held.entry(alarm.clone()).or_default();
				*held += 1;
				if *held >= rule.minutes && !self.said.contains_key(&alarm) {
					let started = at - i64::from(rule.minutes - 1) * MINUTE;
					let since = Timestamp::from_millisecond(started).unwrap_or(Timestamp::UNIX_EPOCH);
					self.said.insert(alarm, Alarm { since });
				}
			}
		}
		self.said != before
	}

	pub fn said(&self) -> &Alarms {
		&self.said
	}
}

/// Each alarm `rule` judges in `readings`, with its value: the mean, or its percent of the total.
fn judged(rule: &Rule, readings: &Readings, info: Option<&Value>) -> Vec<(String, f64)> {
	let total = rule.of.map(|of| info.and_then(|info| info.get(of)).and_then(Value::as_f64));
	readings
		.iter()
		.filter(|(metric, _)| {
			*metric == rule.metric
				|| metric.strip_prefix(rule.metric).is_some_and(|rest| rest.starts_with('.'))
		})
		.filter_map(|(metric, gauge)| {
			let value = match total {
				None => gauge.mean,
				Some(Some(total)) if total > 0.0 => gauge.mean / total * 100.0,
				Some(_) => return None,
			};
			let alarm = if metric == rule.metric { rule.alarm.to_owned() } else { metric.clone() };
			Some((alarm, value))
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::history::Gauge;
	use serde_json::json;

	/// The minute `minute` minutes into 2026-10-09, in milliseconds.
	fn at(minute: i64) -> i64 {
		1_791_504_000_000 + minute * MINUTE
	}

	fn read(pairs: &[(&str, f64)]) -> Readings {
		pairs
			.iter()
			.map(|(metric, mean)| ((*metric).to_owned(), Gauge { mean: *mean, max: *mean }))
			.collect()
	}

	#[test]
	fn a_reading_between_the_two_lines_neither_sets_nor_clears() {
		let mut judge = Judge::default();
		for minute in 0..5 {
			judge.minute(at(minute), &read(&[("cpu.usage", 95.0)]), None);
		}
		assert!(judge.said().contains_key("cpu"));
		// At 87, under the line it was said at and over the one it clears at.
		assert!(!judge.minute(at(5), &read(&[("cpu.usage", 87.0)]), None));
		assert!(judge.minute(at(6), &read(&[("cpu.usage", 84.9)]), None));
		// Back between the lines, it is not said again until it has held five minutes over 90.
		for minute in 7..12 {
			assert!(!judge.minute(at(minute), &read(&[("cpu.usage", 87.0)]), None));
		}
	}

	#[test]
	fn the_processor_is_said_after_five_minutes_high_and_cleared_by_one_minute_under() {
		let mut judge = Judge::default();
		for minute in 0..4 {
			assert!(!judge.minute(at(minute), &read(&[("cpu.usage", 95.0)]), None));
		}
		assert!(judge.minute(at(4), &read(&[("cpu.usage", 91.0)]), None));
		let since = Timestamp::from_millisecond(at(0)).unwrap();
		assert_eq!(judge.said()["cpu"], Alarm { since });
		assert!(!judge.minute(at(5), &read(&[("cpu.usage", 99.0)]), None));
		// A minute without the meter says nothing either way.
		assert!(!judge.minute(at(6), &Readings::new(), None));
		assert!(judge.minute(at(7), &read(&[("cpu.usage", 84.9)]), None));
		assert!(judge.said().is_empty());
	}

	#[test]
	fn memory_and_storage_are_shares_of_the_machine_and_storage_is_said_at_once() {
		let info = json!({ "memory": 1000.0, "storage": 2000.0 });
		let mut judge = Judge::default();
		let full = read(&[("memory.used", 950.0), ("storage.used", 1900.0)]);
		assert!(judge.minute(at(0), &full, Some(&info)));
		assert_eq!(judge.said().keys().collect::<Vec<_>>(), ["storage"]);
		for minute in 1..4 {
			judge.minute(at(minute), &full, Some(&info));
		}
		assert!(judge.minute(at(4), &full, Some(&info)));
		assert_eq!(judge.said().keys().collect::<Vec<_>>(), ["memory", "storage"]);
		// Without the machine's totals there is no share to judge.
		let mut blind = Judge::default();
		assert!(!blind.minute(at(0), &full, None));
	}

	#[test]
	fn each_temperature_zone_is_judged_apart() {
		let mut judge = Judge::default();
		let hot = read(&[("temperature.soc", 86.0), ("temperature.gpu", 70.0)]);
		for minute in 0..5 {
			judge.minute(at(minute), &hot, None);
		}
		assert_eq!(judge.said().keys().collect::<Vec<_>>(), ["temperature.soc"]);
	}
}
