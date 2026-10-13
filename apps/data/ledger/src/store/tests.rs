use super::testing::store;
use super::*;
use ledger::Record;

fn record(id: &str, state: State, finished_at: Option<&str>) -> Record {
	Record {
		service: "shot".into(),
		id: id.into(),
		kind: "capture".into(),
		state,
		caller: Caller::Public,
		asked_at: "2026-09-28T12:00:00Z".parse().unwrap(),
		started_at: None,
		finished_at: finished_at.map(|at| at.parse().unwrap()),
		summary: serde_json::json!({}),
		detail: None,
	}
}

fn task(id: &str, state: State, finished_at: Option<&str>) -> Task {
	record(id, state, finished_at).into()
}

fn asked_at(id: &str, state: State, asked_at: &str, finished_at: Option<&str>) -> Task {
	Task {
		record: Record { asked_at: asked_at.parse().unwrap(), ..record(id, state, finished_at) },
		parent: None,
	}
}

fn event(task: &str, seq: u64) -> Event {
	Event {
		service: "shot".into(),
		task: task.into(),
		seq,
		at: "2026-09-28T12:00:00Z".parse().unwrap(),
		stage: "resolving".into(),
		level: Level::Info,
		message: "starting".into(),
		data: serde_json::Value::Null,
	}
}

#[test]
fn a_batch_keeps_one_of_each_task_as_the_rule_would_in_turn() {
	let done = task("a", State::Done, Some("2026-09-28T12:05:00Z"));
	let late = task("a", State::Running, Some("2026-09-28T12:04:00Z"));
	let kept = winners(vec![
		task("a", State::Queued, None),
		done.clone(),
		late,
		task("b", State::Queued, None),
	]);
	assert_eq!(kept.len(), 2);
	assert_eq!((kept[0].record.id.as_str(), kept[0].record.state), ("a", State::Done));
	assert_eq!(kept[1].record.id, "b");
	// Asked again later: replaces even a finished one.
	let again = asked_at("a", State::Queued, "2026-09-28T13:00:00Z", None);
	assert_eq!(winners(vec![done, again])[0].record.state, State::Queued);
}

#[tokio::test]
async fn a_batch_sending_one_task_several_times_keeps_what_the_rule_keeps() {
	let Some(store) = store().await else { return };
	let items = vec![
		Item::Task(task("a", State::Queued, None)),
		Item::Task(task("a", State::Done, Some("2026-09-28T12:05:00Z"))),
		Item::Task(task("a", State::Running, Some("2026-09-28T12:04:00Z"))),
		Item::Event(event("a", 1)),
		Item::Event(event("a", 1)),
	];
	assert_eq!(store.batch(items).await.unwrap(), 5);
	let view = store.view("shot", "a").await.unwrap().unwrap();
	assert_eq!(view.task.task.record.state, State::Done);
	assert_eq!(view.events.len(), 1);
}

#[tokio::test(flavor = "multi_thread")]
async fn a_batch_is_one_round_trip_however_many_items_it_carries() {
	let Some(store) = super::testing::store_behind(std::time::Duration::from_millis(25)).await else {
		return;
	};
	// The connection and its prepared statement first, as a running ledger has them.
	store.batch(vec![Item::Event(event("warm", 1))]).await.unwrap();
	let mut items = Vec::new();
	for n in 0..100 {
		let id = format!("t{n}");
		items.push(Item::Task(task(&id, State::Running, None)));
		items.push(Item::Task(task(&id, State::Done, Some("2026-09-28T12:05:00Z"))));
		items.push(Item::Event(event(&id, 1)));
	}
	let started = std::time::Instant::now();
	assert_eq!(store.batch(items).await.unwrap(), 300);
	let took = started.elapsed();
	eprintln!("a batch of 300 items over 50 ms round trips took {took:?}");
	// 50 ms a round trip: 300 of them would be 15 s.
	assert!(took < std::time::Duration::from_millis(500), "{took:?}");
	let done = Filter { state: Some("done".into()), ..Filter::default() };
	assert_eq!(store.list(&done, None, 500).await.unwrap().len(), 100);
}

#[tokio::test]
async fn a_task_and_a_batch_commit_locally_and_nothing_else_does() {
	let Some(store) = store().await else { return };
	let mut client = store.pool.get().await.unwrap();
	let setting = "SELECT current_setting('synchronous_commit')";
	let read = async |client: &deadpool_postgres::Transaction<'_>| -> String {
		client.query_one(setting, &[]).await.unwrap().get(0)
	};
	// Each in a transaction held open, so the setting is read before the commit lets it go.
	let transaction = client.transaction().await.unwrap();
	upsert_in(&transaction, task("local", State::Running, None)).await.unwrap();
	assert_eq!(read(&transaction).await, "local");
	transaction.rollback().await.unwrap();
	let transaction = client.transaction().await.unwrap();
	let (tasks, events) = (TaskColumns::default(), EventColumns::default());
	let params: Vec<&(dyn ToSql + Sync)> =
		tasks.params().into_iter().chain(events.params()).collect();
	transaction.query_one(BATCH.as_str(), &params).await.unwrap();
	assert_eq!(read(&transaction).await, "local");
	transaction.rollback().await.unwrap();
	// Alone, the statement is its own transaction, and the setting goes with it: whatever the
	// session does next waits for the cluster's quorum again.
	upsert_in(&client, task("local", State::Running, None)).await.unwrap();
	let after: String = client.query_one(setting, &[]).await.unwrap().get(0);
	assert_eq!(after, "on");
	assert_eq!(store.get("shot", "local").await.unwrap().unwrap().task.record.id, "local");
}

#[test]
fn a_cursor_round_trips_through_its_encoding() {
	let cursor = Cursor { updated_at: 12345, id: "with:colon".into() };
	assert_eq!(Cursor::parse(&cursor.encode()), Some(cursor));
}

#[tokio::test]
async fn a_late_running_does_not_undo_a_done() {
	let Some(store) = store().await else { return };
	store.upsert(task("a", State::Done, Some("2026-09-28T12:05:00Z"))).await.unwrap();
	let kept = store.upsert(task("a", State::Running, Some("2026-09-28T12:04:00Z"))).await.unwrap();
	assert_eq!(kept.task.record.state, State::Done);
	assert_eq!(store.get("shot", "a").await.unwrap().unwrap().task.record.state, State::Done);
}

#[tokio::test]
async fn a_record_without_finished_at_never_replaces_one_that_has_it() {
	let Some(store) = store().await else { return };
	store.upsert(task("a", State::Done, Some("2026-09-28T12:05:00Z"))).await.unwrap();
	let kept = store.upsert(task("a", State::Running, None)).await.unwrap();
	assert_eq!(kept.task.record.state, State::Done);
}

#[tokio::test]
async fn a_later_asked_at_always_replaces_the_kept_one() {
	let Some(store) = store().await else { return };
	let failed = asked_at("a", State::Failed, "2026-09-28T12:00:00Z", Some("2026-09-28T12:00:00Z"));
	store.upsert(failed.clone()).await.unwrap();
	let kept =
		store.upsert(asked_at("a", State::Queued, "2026-09-28T13:00:00Z", None)).await.unwrap();
	assert_eq!(kept.task.record.state, State::Queued);

	let failed = Task { record: Record { id: "b".into(), ..failed.record }, parent: None };
	store.upsert(failed).await.unwrap();
	let kept =
		store.upsert(asked_at("b", State::Queued, "2026-09-28T12:00:00Z", None)).await.unwrap();
	assert_eq!(kept.task.record.state, State::Failed);
}

#[tokio::test]
async fn a_later_finished_at_replaces_the_kept_one() {
	let Some(store) = store().await else { return };
	store.upsert(task("a", State::Running, Some("2026-09-28T12:00:00Z"))).await.unwrap();
	let kept = store.upsert(task("a", State::Done, Some("2026-09-28T12:05:00Z"))).await.unwrap();
	assert_eq!(kept.task.record.state, State::Done);
}

#[tokio::test]
async fn sending_the_same_record_twice_is_harmless() {
	let Some(store) = store().await else { return };
	let first = store.upsert(task("a", State::Queued, None)).await.unwrap();
	let second = store.upsert(task("a", State::Queued, None)).await.unwrap();
	assert_eq!(first.task, second.task);
}

#[tokio::test]
async fn the_rule_holds_for_writes_that_race() {
	let Some(store) = store().await else { return };
	let store = std::sync::Arc::new(store);
	// Each pair races a `done` against a late `running` of the same asking, many times over.
	let mut racing = Vec::new();
	for n in 0..40 {
		let id = format!("race-{}", (n / 2) % 10);
		let (store, late) = (store.clone(), n % 2 == 0);
		racing.push(tokio::spawn(async move {
			let (state, at) = if late {
				(State::Running, "2026-09-28T12:04:00Z")
			} else {
				(State::Done, "2026-09-28T12:05:00Z")
			};
			store.upsert(task(&id, state, Some(at))).await.unwrap();
		}));
	}
	for racer in racing {
		racer.await.unwrap();
	}
	for n in 0..10 {
		let kept = store.get("shot", &format!("race-{n}")).await.unwrap().unwrap();
		assert_eq!(kept.task.record.state, State::Done, "race-{n}");
	}
}

#[tokio::test]
async fn lists_newest_updated_at_first_paged_and_filtered() {
	let Some(store) = store().await else { return };
	for id in ["a", "b", "c"] {
		store.upsert(task(id, State::Done, Some("2026-09-28T12:00:00Z"))).await.unwrap();
	}
	store.upsert(task("d", State::Failed, Some("2026-09-28T12:00:00Z"))).await.unwrap();

	let all = store.list(&Filter::default(), None, 10).await.unwrap();
	assert_eq!(all.len(), 4);
	assert_eq!(all[0].task.record.id, "d");
	assert_eq!(all[3].task.record.id, "a");

	let page = store.list(&Filter::default(), None, 2).await.unwrap();
	assert_eq!(page.len(), 2);
	let cursor = Cursor { updated_at: nanos(page[1].updated_at), id: page[1].task.record.id.clone() };
	let rest = store.list(&Filter::default(), Some(&cursor), 10).await.unwrap();
	assert_eq!(rest.len(), 2);
	assert_eq!(rest[0].task.record.id, all[2].task.record.id);

	let failed = Filter { state: Some("failed".into()), ..Filter::default() };
	let failed = store.list(&failed, None, 10).await.unwrap();
	assert_eq!(failed.len(), 1);
	assert_eq!(failed[0].task.record.id, "d");
	let nothing = Filter { state: Some("sleeping".into()), ..Filter::default() };
	assert!(store.list(&nothing, None, 10).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_batch_stores_a_task_and_its_events_once_even_sent_twice() {
	let Some(store) = store().await else { return };
	let items = vec![
		Item::Task(task("a", State::Running, None)),
		Item::Event(event("a", 1)),
		Item::Event(event("a", 2)),
	];
	assert_eq!(store.batch(items.clone()).await.unwrap(), 3);
	assert_eq!(store.batch(items).await.unwrap(), 3);

	let view = store.view("shot", "a").await.unwrap().unwrap();
	assert_eq!(view.task.task.record.state, State::Running);
	assert_eq!(view.events.len(), 2);
	assert_eq!(view.events[0].seq, 1);
	assert_eq!(view.events[1].seq, 2);
}

#[tokio::test]
async fn events_answer_in_seq_order_however_they_arrived() {
	let Some(store) = store().await else { return };
	store.upsert(task("a", State::Running, None)).await.unwrap();
	// The size a real seq has: a moment in nanoseconds.
	let base = 1_790_000_000_000_000_000;
	let arrived = [5, 1, 3].map(|n| Item::Event(event("a", base + n)));
	store.batch(arrived.to_vec()).await.unwrap();
	let view = store.view("shot", "a").await.unwrap().unwrap();
	let seqs: Vec<u64> = view.events.iter().map(|event| event.seq - base).collect();
	assert_eq!(seqs, vec![1, 3, 5]);
}

#[tokio::test]
async fn the_task_view_carries_its_children_newest_first() {
	let Some(store) = store().await else { return };
	store.upsert(task("parent", State::Running, None)).await.unwrap();
	for id in ["child-a", "child-b"] {
		let mut child = task(id, State::Running, None);
		child.parent = Some(ledger::Parent { service: "shot".into(), id: "parent".into() });
		store.upsert(child).await.unwrap();
	}
	let view = store.view("shot", "parent").await.unwrap().unwrap();
	assert_eq!(view.children.len(), 2);
	assert_eq!(view.children[0].task.record.id, "child-b");
	assert_eq!(view.children[1].task.record.id, "child-a");
}

#[tokio::test]
async fn the_upsert_rule_still_holds_through_a_batch() {
	let Some(store) = store().await else { return };
	let done = Item::Task(task("a", State::Done, Some("2026-09-28T12:05:00Z")));
	store.batch(vec![done]).await.unwrap();
	let late = Item::Task(task("a", State::Running, Some("2026-09-28T12:04:00Z")));
	store.batch(vec![late]).await.unwrap();
	assert_eq!(store.get("shot", "a").await.unwrap().unwrap().task.record.state, State::Done);
}

/// A task `hours_ago` hours before now, for `service` under `id` and `state`. See
/// spec/architecture/ledger.md, "Counted for telemetry".
fn asked_hours_ago(service: &str, id: &str, state: State, hours_ago: i64) -> Task {
	let asked_at = Timestamp::now().checked_sub(jiff::SignedDuration::from_hours(hours_ago)).unwrap();
	Task {
		record: Record { service: service.into(), asked_at, ..record(id, state, None) },
		parent: None,
	}
}

#[tokio::test]
async fn counts_group_by_service_hour_bucket_and_state() {
	let Some(store) = store().await else { return };
	store.upsert(asked_hours_ago("shot", "a", State::Done, 0)).await.unwrap();
	store.upsert(asked_hours_ago("shot", "b", State::Done, 0)).await.unwrap();
	store.upsert(asked_hours_ago("shot", "c", State::Failed, 0)).await.unwrap();
	store.upsert(asked_hours_ago("geo", "d", State::Done, 0)).await.unwrap();

	let counts = store.counts(1).await.unwrap();
	let count = |service: &str, state: &str| {
		counts.iter().find(|row| row.service == service && row.state == state).map(|row| row.count)
	};
	assert_eq!(count("shot", "done"), Some(2));
	assert_eq!(count("shot", "failed"), Some(1));
	assert_eq!(count("geo", "done"), Some(1));
}

#[tokio::test]
async fn the_window_covers_the_last_hours_plus_the_one_under_way() {
	let Some(store) = store().await else { return };
	store.upsert(asked_hours_ago("shot", "now", State::Done, 0)).await.unwrap();
	store.upsert(asked_hours_ago("shot", "one-ago", State::Done, 1)).await.unwrap();
	store.upsert(asked_hours_ago("shot", "two-ago", State::Done, 2)).await.unwrap();
	let total = |counts: Vec<Count>| counts.iter().map(|row| row.count).sum::<i64>();
	assert_eq!(total(store.counts(1).await.unwrap()), 2);
	assert_eq!(total(store.counts(2).await.unwrap()), 3);
	assert!(store.counts(24).await.unwrap().iter().all(|row| row.service == "shot"));
}

#[tokio::test]
async fn migrating_again_changes_nothing() {
	let Some(store) = store().await else { return };
	assert!(store.migrate().await.unwrap().is_empty());
}
