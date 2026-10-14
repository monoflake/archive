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

/// How long a stopping registry waits for the ledger to take what it still holds.
pub const FLUSH: std::time::Duration = ledger::STOPPING;

/// What `ledger` still holds sent before the registry stops, within `within`; how many items were
/// not. `Ledger::stopping` says them.
pub async fn stopping(ledger: Option<&Ledger>, within: std::time::Duration) -> usize {
	let Some(ledger) = ledger else { return 0 };
	ledger.stopping("registry", within).await
}

/// The task a write is: every instant its own, since it was asked, started and finished at once.
pub fn record(kind: Kind, written: &Written) -> Record {
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
			"name": written.name,
			"revision": written.revision,
		}),
		detail: None,
	}
}

/// The one line its event says, naming the resource by its name or, having none, its id.
pub fn message(kind: Kind, by: &str, written: &Written) -> String {
	let named = written.name.as_deref().unwrap_or(&written.id);
	format!("{by} wrote {} {named} at revision {}", kind.name(), written.revision)
}

/// The task and its one event, `written`, handed to `ledger`.
pub fn tell(ledger: &Ledger, kind: Kind, by: &str, written: &Written) {
	ledger.record(record(kind, written));
	let message = message(kind, by, written);
	ledger.task(SERVICE, &written.task).event("written", Level::Info, message, Value::Null);
}

#[cfg(test)]
mod tests {
	use super::*;

	fn written() -> Written {
		Written {
			revision: 42,
			id: "0123456789abcdef".into(),
			name: Some("tyo".into()),
			task: "fedcba9876543210".into(),
			at: "2026-10-13T12:00:00Z".parse().unwrap(),
		}
	}

	#[test]
	fn a_write_is_a_task_done_at_its_instant() {
		let record = record(Kind::Node, &written());
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

	/// A ledger on 127.0.0.1 keeping the ids of every task it is sent.
	async fn ledger() -> (String, std::sync::Arc<std::sync::Mutex<Vec<String>>>) {
		let taken: std::sync::Arc<std::sync::Mutex<Vec<String>>> = Default::default();
		let kept = taken.clone();
		let events = axum::routing::post(move |axum::Json(batch): axum::Json<Vec<Value>>| {
			let kept = kept.clone();
			async move {
				let ids = batch.iter().filter_map(|item| item["task"]["id"].as_str().map(str::to_owned));
				kept.lock().unwrap().extend(ids);
				axum::http::StatusCode::OK
			}
		});
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let address = listener.local_addr().unwrap();
		let router = axum::Router::new().route("/events", events);
		tokio::spawn(async move { axum::serve(listener, router).await });
		(format!("http://{address}"), taken)
	}

	/// What a write handed over just before the registry stops still reaches the ledger.
	#[tokio::test]
	async fn a_stopping_registry_sends_what_it_holds() {
		let (base, taken) = ledger().await;
		let ledger = Ledger::to(base);
		tell(&ledger, Kind::Node, "test", &written());
		assert_eq!(stopping(Some(&ledger), FLUSH).await, 0);
		assert_eq!(*taken.lock().unwrap(), ["fedcba9876543210"]);
		assert_eq!(stopping(None, FLUSH).await, 0);
	}

	/// A ledger that does not answer holds the stop no longer than its bound, and the items left
	/// are counted.
	#[tokio::test]
	async fn a_stopping_registry_waits_no_longer_than_its_bound() {
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let address = listener.local_addr().unwrap();
		drop(listener);
		let ledger = Ledger::to(format!("http://{address}"));
		tell(&ledger, Kind::Node, "test", &written());
		let started = std::time::Instant::now();
		let within = std::time::Duration::from_millis(400);
		assert_eq!(stopping(Some(&ledger), within).await, 2, "the task and its event");
		assert!(started.elapsed() < std::time::Duration::from_secs(2));
	}

	#[test]
	fn its_event_says_who_wrote_what_in_one_line() {
		let web = Written { name: Some("web".into()), ..written() };
		assert_eq!(message(Kind::App, "import", &web), "import wrote app web at revision 42");
		let unnamed = Written { name: None, ..written() };
		let said = message(Kind::Account, "alice", &unnamed);
		assert_eq!(said, "alice wrote account 0123456789abcdef at revision 42");
	}

	#[test]
	fn an_unnamed_resource_is_summarized_with_a_null_name() {
		let unnamed = Written { name: None, ..written() };
		assert_eq!(record(Kind::Account, &unnamed).summary["name"], Value::Null);
	}
}
