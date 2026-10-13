//! A write that changed something, told as a task to the ledger `Ledger::start` finds, as every
//! service's is: whole when answered, so recorded `done` with one event, handed over and never
//! waited on. A registry built without one, as in a test, tells nowhere. See
//! spec/architecture/registry.md, "A write that changes something is a task", and
//! spec/architecture/ledger.md, "Pushed to, never asking".

use crate::kinds::Kind;
use crate::store::Written;
use ledger::{Caller, Ledger, Level, Record, State};
use serde_json::{Value, json};

/// The service every task of the registry is told as.
pub const SERVICE: &str = "registry";

/// The task a write is: every instant its own, since it was asked, started and finished at once.
pub fn record(kind: Kind, name: &str, written: &Written) -> Record {
	Record {
		service: SERVICE.to_owned(),
		id: written.task.clone(),
		kind: "write".to_owned(),
		state: State::Done,
		caller: Caller::Ours,
		asked_at: written.at,
		started_at: Some(written.at),
		finished_at: Some(written.at),
		summary: json!({
			"kind": kind.name(),
			"id": written.id,
			"name": name,
			"revision": written.revision,
		}),
		detail: None,
	}
}

/// The one line its event says.
pub fn message(kind: Kind, name: &str, by: &str, written: &Written) -> String {
	format!("{by} wrote {} {name} at revision {}", kind.name(), written.revision)
}

/// The task and its one event, `written`, handed to `ledger`.
pub fn tell(ledger: &Ledger, kind: Kind, name: &str, by: &str, written: &Written) {
	ledger.record(record(kind, name, written));
	let message = message(kind, name, by, written);
	ledger.task(SERVICE, &written.task).event("written", Level::Info, message, Value::Null);
}

#[cfg(test)]
mod tests {
	use super::*;

	fn written() -> Written {
		Written {
			revision: 42,
			id: "0123456789abcdef".into(),
			task: "fedcba9876543210".into(),
			at: "2026-10-13T12:00:00Z".parse().unwrap(),
		}
	}

	#[test]
	fn a_write_is_a_task_done_at_its_instant() {
		let record = record(Kind::Node, "tyo", &written());
		let wire = serde_json::to_value(&record).unwrap();
		assert_eq!(
			wire,
			json!({
				"service": "registry",
				"id": "fedcba9876543210",
				"kind": "write",
				"state": "done",
				"caller": "ours",
				"asked_at": "2026-10-13T12:00:00Z",
				"started_at": "2026-10-13T12:00:00Z",
				"finished_at": "2026-10-13T12:00:00Z",
				"summary": {
					"kind": "node",
					"id": "0123456789abcdef",
					"name": "tyo",
					"revision": 42,
				},
			})
		);
	}

	#[test]
	fn its_event_says_who_wrote_what_in_one_line() {
		let message = message(Kind::App, "web", "import", &written());
		assert_eq!(message, "import wrote app web at revision 42");
	}
}
