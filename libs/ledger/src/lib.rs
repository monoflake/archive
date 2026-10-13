//! The ledger's model of a task and its events, and the client every service hands them to. An
//! item is handed over and the call returns at once; a background task batches what is queued and
//! sends it to `POST {base}/events`, keeps what could not be sent in a bounded queue with the
//! oldest dropped first, and tries again with backoff -- deliver.rs, which says once per streak
//! whenever a record is held or lost. A ledger that is down costs records, never the work, and a
//! service stopping flushes what it holds within a bound. See spec/architecture/ledger.md.

mod deliver;

use deliver::Message;
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum State {
	Queued,
	Running,
	Done,
	Failed,
}

/// Who asked: the public, as the gateway marked the request, or one of ours.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Caller {
	Public,
	Ours,
}

/// One task as its service sees it now; the ledger keeps the latest it was sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
	pub service: String,
	pub id: String,
	/// What was asked, in the service's own words: `capture`.
	pub kind: String,
	pub state: State,
	pub caller: Caller,
	pub asked_at: Timestamp,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub started_at: Option<Timestamp>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub finished_at: Option<Timestamp>,
	/// A small object the service chooses: for `shot`, the page's URL.
	#[serde(default)]
	pub summary: serde_json::Value,
	/// Why it failed, when it did.
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub detail: Option<String>,
}

/// A record as the ledger answers with it: what was sent, and when the ledger last took it.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Stored {
	#[serde(flatten)]
	pub record: Record,
	pub updated_at: Timestamp,
}

/// The task that asked for this one, when one did. See spec/architecture/ledger.md, "`parent` ties
/// a chain together across services".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Parent {
	pub service: String,
	pub id: String,
}

/// A `Record` plus the task that asked for it, when one did. `Record` itself keeps the field set
/// `apps/compute/shot` and `apps/data/ledger` already construct by struct literal (see this
/// crate's module docs in the worker report for why); `Task` carries `parent` alongside it,
/// flattened, so a task with no parent serializes exactly as `Record` does. `Ledger::record` wraps
/// a bare `Record` into one of these with no parent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Task {
	#[serde(flatten)]
	pub record: Record,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub parent: Option<Parent>,
}

impl From<Record> for Task {
	fn from(record: Record) -> Self {
		Self { record, parent: None }
	}
}

/// How severe a step was.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Level {
	Info,
	Warn,
	Error,
}

/// One step of a task, appended and never changed. See spec/architecture/ledger.md, "A task, and
/// the events that make it up".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Event {
	pub service: String,
	pub task: String,
	/// The event's place within its task: only ever grows. See "Order is `seq`, never arrival".
	pub seq: u64,
	pub at: Timestamp,
	/// The step, in the service's own words: `resolving`, `loading`, `rendering`, `storing`.
	pub stage: String,
	pub level: Level,
	/// One line for a person.
	pub message: String,
	/// A small object, when the step has figures worth keeping.
	#[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
	pub data: serde_json::Value,
}

/// What `POST /events` takes: a batch is a JSON array of `Item`, each one either a task's whole
/// record or one event, externally tagged and lowercase. See the `tests` module below for the
/// wire shape, and spec/architecture/ledger.md, "Pushed to, never asking".
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Item {
	Task(Task),
	Event(Event),
}

/// How many items (tasks and events alike) wait while the ledger cannot be reached; past it the
/// oldest is dropped.
pub const WAITING: usize = 1000;

/// How many items one `POST /events` call carries at most.
const BATCH: usize = 200;

/// The first wait after a failed send, doubled each time up to the last.
const BACKOFF: (Duration, Duration) = (Duration::from_secs(1), Duration::from_secs(60));

/// How many tasks' last `seq` are kept before the oldest is pruned, so a process making events for
/// many tasks does not grow this map without bound.
const SEQS: usize = 10_000;

/// The last `seq` handed out per `(service, task)`, bounded by insertion order.
struct SeqTracker {
	last: HashMap<(String, String), u64>,
	order: VecDeque<(String, String)>,
}

impl SeqTracker {
	fn new() -> Self {
		Self { last: HashMap::new(), order: VecDeque::new() }
	}

	/// The moment in nanoseconds, or one past this task's last `seq` when the clock has not moved
	/// past it. See spec/architecture/ledger.md, "Order is `seq`, never arrival".
	fn next(&mut self, service: &str, task: &str) -> u64 {
		self.next_at(service, task, Timestamp::now().as_nanosecond() as u64)
	}

	fn next_at(&mut self, service: &str, task: &str, now: u64) -> u64 {
		let key = (service.to_owned(), task.to_owned());
		let seq = match self.last.get(&key) {
			Some(&last) if last >= now => last + 1,
			_ => now,
		};
		if !self.last.contains_key(&key) {
			self.order.push_back(key.clone());
		}
		self.last.insert(key, seq);
		while self.order.len() > SEQS {
			if let Some(oldest) = self.order.pop_front() {
				self.last.remove(&oldest);
			}
		}
		seq
	}
}

/// The handle a service keeps: cheap to clone, and never waits but to flush.
#[derive(Clone)]
pub struct Ledger {
	sender: mpsc::UnboundedSender<Message>,
	seqs: Arc<Mutex<SeqTracker>>,
	/// How many items the background task holds, unsent.
	held: Arc<AtomicUsize>,
}

impl Ledger {
	/// Start sending to the ledger at `base`: `LEDGER_URL` when it is set, the platform's ledger
	/// otherwise. Needs a Tokio runtime.
	pub fn start() -> Self {
		let base =
			std::env::var("LEDGER_URL").unwrap_or_else(|_| monoflake::INTERNAL_LEDGER.to_owned());
		Self::to(base)
	}

	pub fn to(base: String) -> Self {
		Self::logging(base, Arc::new(|line: &str| eprintln!("{line}")))
	}

	/// `to`, its lines said to `log` rather than stderr.
	pub(crate) fn logging(base: String, log: deliver::Log) -> Self {
		let (sender, receiver) = mpsc::unbounded_channel();
		let held = Arc::new(AtomicUsize::new(0));
		let base = base.trim_end_matches('/').to_owned();
		tokio::spawn(deliver::deliver(base, receiver, held.clone(), log));
		Self { sender, seqs: Arc::new(Mutex::new(SeqTracker::new())), held }
	}

	/// Hand a task over. It is sent in the background, and a task that cannot be is lost rather
	/// than waited for.
	pub fn send(&self, task: Task) {
		let _ = self.sender.send(Message::Item(Box::new(Item::Task(task))));
	}

	/// Everything handed over and not yet sent, sent now, past any backoff, for a service that is
	/// stopping: waited on for `within` at most, and answered with how many items are still unsent.
	/// What is unsent then is lost with the process, so the caller says how many.
	pub async fn flush(&self, within: Duration) -> usize {
		let deadline = tokio::time::Instant::now() + within;
		let (answer, answered) = oneshot::channel();
		if self.sender.send(Message::Flush { deadline, answer }).is_err() {
			return self.held.load(Ordering::Relaxed);
		}
		// A moment past the bound, for the answer the background task gives at the bound.
		let grace = deadline + Duration::from_millis(250);
		match tokio::time::timeout_at(grace, answered).await {
			Ok(Ok(held)) => held,
			_ => self.held.load(Ordering::Relaxed),
		}
	}

	/// Shorthand for `send` with a parentless task.
	pub fn record(&self, record: Record) {
		self.send(record.into());
	}

	/// A handle for one task's events, cheap to clone.
	pub fn task(&self, service: impl Into<String>, id: impl Into<String>) -> TaskEvents {
		TaskEvents { service: service.into(), task: id.into(), ledger: self.clone() }
	}
}

/// One task's events, made through `Ledger::task`.
#[derive(Clone)]
pub struct TaskEvents {
	service: String,
	task: String,
	ledger: Ledger,
}

impl TaskEvents {
	/// Make and hand over one event; its `seq` is assigned here, not by the ledger.
	pub fn event(
		&self,
		stage: impl Into<String>,
		level: Level,
		message: impl Into<String>,
		data: serde_json::Value,
	) {
		let seq = self.ledger.seqs.lock().unwrap().next(&self.service, &self.task);
		let event = Event {
			service: self.service.clone(),
			task: self.task.clone(),
			seq,
			at: Timestamp::now(),
			stage: stage.into(),
			level,
			message: message.into(),
			data,
		};
		let _ = self.ledger.sender.send(Message::Item(Box::new(Item::Event(event))));
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn record(id: &str) -> Record {
		Record {
			service: "shot".into(),
			id: id.into(),
			kind: "capture".into(),
			state: State::Queued,
			caller: Caller::Public,
			asked_at: "2026-09-28T12:00:00Z".parse().unwrap(),
			started_at: None,
			finished_at: None,
			summary: serde_json::json!({ "url": "https://example.com/" }),
			detail: None,
		}
	}

	#[test]
	fn a_record_reads_as_the_ledger_documents_it() {
		let text = serde_json::to_string(&record("a")).unwrap();
		assert_eq!(
			text,
			r#"{"service":"shot","id":"a","kind":"capture","state":"queued","caller":"public","asked_at":"2026-09-28T12:00:00Z","summary":{"url":"https://example.com/"}}"#
		);
		assert_eq!(serde_json::from_str::<Record>(&text).unwrap(), record("a"));
	}

	#[test]
	fn a_batch_reads_as_the_ledger_documents_it() {
		let items = vec![
			Item::Task(record("a").into()),
			Item::Event(Event {
				service: "shot".into(),
				task: "a".into(),
				seq: 1,
				at: "2026-09-28T12:00:01Z".parse().unwrap(),
				stage: "resolving".into(),
				level: Level::Info,
				message: "starting".into(),
				data: serde_json::Value::Null,
			}),
		];
		let text = serde_json::to_string(&items).unwrap();
		assert_eq!(
			text,
			r#"[{"task":{"service":"shot","id":"a","kind":"capture","state":"queued","caller":"public","asked_at":"2026-09-28T12:00:00Z","summary":{"url":"https://example.com/"}}},{"event":{"service":"shot","task":"a","seq":1,"at":"2026-09-28T12:00:01Z","stage":"resolving","level":"info","message":"starting"}}]"#
		);
	}

	#[test]
	fn a_task_with_a_parent_carries_it_alongside_records_fields() {
		let task =
			Task { record: record("a"), parent: Some(Parent { service: "cron".into(), id: "p".into() }) };
		let text = serde_json::to_string(&task).unwrap();
		assert!(text.contains(r#""parent":{"service":"cron","id":"p"}"#));
	}

	#[tokio::test]
	async fn send_queues_a_task_with_its_parent_on_the_wire() {
		let (sender, mut receiver) = mpsc::unbounded_channel();
		let seqs = Arc::new(Mutex::new(SeqTracker::new()));
		let ledger = Ledger { sender, seqs, held: Arc::default() };
		let task =
			Task { record: record("a"), parent: Some(Parent { service: "cron".into(), id: "p".into() }) };
		ledger.send(task.clone());
		let Some(Message::Item(item)) = receiver.recv().await else { panic!("an item") };
		assert_eq!(*item, Item::Task(task));
		assert!(
			serde_json::to_string(&item).unwrap().contains(r#""parent":{"service":"cron","id":"p"}"#)
		);
	}

	#[test]
	fn seq_grows_even_within_one_nanosecond() {
		let mut seqs = SeqTracker::new();
		let a = seqs.next_at("shot", "x", 100);
		let b = seqs.next_at("shot", "x", 100);
		let c = seqs.next_at("shot", "x", 100);
		assert!(a < b);
		assert!(b < c);
		// A different task at the same instant starts from that instant, not from `x`'s history.
		let first = seqs.next_at("shot", "y", 100);
		assert_eq!(first, 100);
	}

	#[test]
	fn keeps_seq_for_only_so_many_tasks() {
		let mut seqs = SeqTracker::new();
		for n in 0..SEQS + 3 {
			seqs.next_at("shot", &n.to_string(), n as u64);
		}
		assert_eq!(seqs.last.len(), SEQS);
		assert!(!seqs.last.contains_key(&("shot".to_string(), "0".to_string())));
		assert!(seqs.last.contains_key(&("shot".to_string(), (SEQS + 2).to_string())));
	}

	#[test]
	fn keeps_the_newest_when_too_many_wait() {
		let mut waiting = VecDeque::new();
		for n in 0..WAITING + 3 {
			deliver::keep(&mut waiting, Item::Task(record(&n.to_string()).into()));
		}
		assert_eq!(waiting.len(), WAITING);
		match waiting.front().unwrap() {
			Item::Task(task) => assert_eq!(task.record.id, "3"),
			Item::Event(_) => panic!("expected a task"),
		}
	}
}
