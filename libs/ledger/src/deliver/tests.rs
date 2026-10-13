use crate::{Caller, Ledger, Record, State, WAITING};
use axum::Router;
use axum::http::StatusCode;
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};
use std::time::Duration;

fn record(id: &str) -> Record {
	Record {
		service: "registry".into(),
		id: id.into(),
		kind: "write".into(),
		state: State::Done,
		caller: Caller::Ours,
		asked_at: "2026-10-13T16:00:00Z".parse().unwrap(),
		started_at: None,
		finished_at: None,
		summary: serde_json::json!({ "revision": 1 }),
		detail: None,
	}
}

/// A ledger on 127.0.0.1 answering each `POST /events` with the next status of its script, 200
/// once the script runs out, and keeping the ids of every batch it took.
#[derive(Default)]
struct Fake {
	script: VecDeque<u16>,
	taken: Vec<String>,
	posts: usize,
}

type Shared = Arc<Mutex<Fake>>;

async fn ledger(script: &[u16]) -> (String, Shared) {
	let fake: Shared =
		Arc::new(Mutex::new(Fake { script: script.iter().copied().collect(), ..Fake::default() }));
	let kept = fake.clone();
	let events = axum::routing::post(move |axum::Json(batch): axum::Json<Vec<Value>>| {
		let fake = kept.clone();
		async move {
			let mut fake = fake.lock().unwrap();
			fake.posts += 1;
			let status = fake.script.pop_front().unwrap_or(200);
			if status == 200 {
				let ids = batch.iter().filter_map(|item| item["task"]["id"].as_str().map(str::to_owned));
				fake.taken.extend(ids);
			}
			StatusCode::from_u16(status).unwrap()
		}
	});
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	tokio::spawn(async move { axum::serve(listener, Router::new().route("/events", events)).await });
	(format!("http://{address}"), fake)
}

/// A base nothing listens on.
async fn unreachable() -> String {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	drop(listener);
	format!("http://{address}")
}

/// A ledger client whose lines are kept to read.
fn logged(base: String) -> (Ledger, Arc<Mutex<Vec<String>>>) {
	let lines: Arc<Mutex<Vec<String>>> = Arc::default();
	let kept = lines.clone();
	let ledger =
		Ledger::logging(base, Arc::new(move |line: &str| kept.lock().unwrap().push(line.into())));
	(ledger, lines)
}

/// Until `done` holds, within five seconds.
async fn until(done: impl Fn() -> bool) {
	for _ in 0..100 {
		if done() {
			return;
		}
		tokio::time::sleep(Duration::from_millis(50)).await;
	}
	panic!("not within five seconds");
}

fn saying(lines: &Mutex<Vec<String>>, words: &str) -> usize {
	lines.lock().unwrap().iter().filter(|line| line.contains(words)).count()
}

/// A refusal keeps its meaning, the batch dropped, and is said once for its streak, then once
/// when the ledger takes items again.
#[tokio::test]
async fn a_refusal_is_said_once_per_streak_and_its_end_once() {
	let (base, fake) = ledger(&[400, 400, 400]).await;
	let (ledger, lines) = logged(base);
	for (sent, id) in ["a", "b", "c"].into_iter().enumerate() {
		ledger.record(record(id));
		until(|| fake.lock().unwrap().posts > sent).await;
	}
	ledger.record(record("d"));
	until(|| fake.lock().unwrap().taken == ["d"]).await;
	until(|| saying(&lines, "takes items again") == 1).await;
	assert_eq!(lines.lock().unwrap().len(), 2, "{lines:?}");
	assert!(lines.lock().unwrap()[0].contains("refused 1 items with 400; dropped"), "{lines:?}");
	assert!(lines.lock().unwrap()[1].contains("after refusing 3"), "{lines:?}");
	// A line says how many and why, never what a record holds.
	assert!(lines.lock().unwrap().iter().all(|line| !line.contains("revision")), "{lines:?}");
}

/// A ledger not answering is said once, holding what waits; a flush sends it at once, past the
/// backoff, and the answer coming back is said once.
#[tokio::test]
async fn a_flush_sends_what_is_held_past_the_backoff_and_the_streak_is_said_once() {
	let (base, fake) = ledger(&[503]).await;
	let (ledger, lines) = logged(base);
	ledger.record(record("a"));
	until(|| fake.lock().unwrap().posts == 1).await;
	until(|| saying(&lines, "did not answer") == 1).await;
	ledger.record(record("b"));
	let started = tokio::time::Instant::now();
	assert_eq!(ledger.flush(Duration::from_secs(5)).await, 0);
	assert!(started.elapsed() < Duration::from_millis(900), "the first backoff is a second");
	assert_eq!(fake.lock().unwrap().taken, ["a", "b"]);
	assert_eq!(saying(&lines, "did not answer"), 1, "{lines:?}");
	assert_eq!(saying(&lines, "answered again; 2 items sent"), 1, "{lines:?}");
}

/// A flush asked of a ledger that does not answer gives up at its bound, saying what is unsent.
#[tokio::test]
async fn a_flush_to_a_ledger_not_answering_ends_at_its_bound_with_what_is_held() {
	let (ledger, lines) = logged(unreachable().await);
	ledger.record(record("a"));
	ledger.record(record("b"));
	let started = tokio::time::Instant::now();
	assert_eq!(ledger.flush(Duration::from_millis(500)).await, 2);
	assert!(started.elapsed() < Duration::from_secs(2), "{:?}", started.elapsed());
	until(|| saying(&lines, "did not answer") == 1).await;
	assert!(lines.lock().unwrap()[0].contains("holding"), "{lines:?}");
}

/// Nothing held, a flush answers at once.
#[tokio::test]
async fn a_flush_with_nothing_held_answers_at_once() {
	let (base, _) = ledger(&[]).await;
	let (ledger, lines) = logged(base);
	assert_eq!(ledger.flush(Duration::from_secs(1)).await, 0);
	assert!(lines.lock().unwrap().is_empty());
}

/// More than `WAITING` held while the ledger does not answer: the oldest go, said once.
#[tokio::test]
async fn dropping_the_oldest_is_said_once_per_streak() {
	let (ledger, lines) = logged(unreachable().await);
	for n in 0..WAITING + 50 {
		ledger.record(record(&n.to_string()));
	}
	assert_eq!(ledger.flush(Duration::from_millis(300)).await, WAITING);
	until(|| saying(&lines, "dropping the oldest") == 1).await;
	assert_eq!(saying(&lines, "dropping the oldest"), 1, "{lines:?}");
}

/// aws-lc-rs whenever its feature is on, ring when it is on alone: one provider, named, whatever
/// else the build enables. Run with `--no-default-features --features ring`, and with both, too.
#[test]
fn the_provider_is_the_one_the_features_name() {
	let named = if cfg!(feature = "aws-lc-rs") { "AwsLcRs" } else { "Ring" };
	assert_eq!(format!("{:?}", super::provider().secure_random), named);
	assert!(!super::provider().cipher_suites.is_empty());
}
