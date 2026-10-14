//! The coordinator against a real Postgres, driven by the test executor as a node would drive it.
//! Time is moved by the statements `due` and `lapse` run, never waited for.

use super::*;
use crate::api::routes;
use crate::harness::{Executor, Halt, StepError, Workflow, call, workflow};
use crate::store::testing::store;
use axum::Router;
use axum::http::StatusCode;
use serde_json::json;
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

struct World {
	store: Arc<Store>,
	router: Router,
}

async fn world() -> Option<World> {
	let store = Arc::new(store().await?);
	Some(World { router: routes(store.clone()), store })
}

impl World {
	async fn open(&self, id: &str, workflow: &str) {
		let (status, answer) = call(
			&self.router,
			"PUT",
			&format!("/v1/runs/certs/{id}"),
			&json!({ "workflow": workflow, "params": { "domain": "canmi.app" } }),
		)
		.await;
		assert_eq!(status, StatusCode::CREATED, "{answer}");
	}

	async fn view(&self, id: &str) -> Value {
		let (status, answer) =
			call(&self.router, "GET", &format!("/v1/runs/certs/{id}"), &json!({})).await;
		assert_eq!(status, StatusCode::OK, "{answer}");
		answer["data"].clone()
	}

	/// Every sleep, wait and retry is due, and every lease has run out.
	async fn due(&self) {
		self.store.exec("UPDATE runs SET run_at = 0 WHERE status IN ('sleeping', 'waiting')").await;
		self.store.exec("UPDATE steps SET wake_at = 0 WHERE status IN ('sleeping', 'waiting')").await;
	}

	async fn lapse(&self) {
		self.store.exec("UPDATE leases SET expires_at = 0").await;
	}

	async fn state_of(&self, id: &str) -> String {
		self.view(id).await["run"]["status"].as_str().unwrap().to_owned()
	}
}

/// The lease a turn was granted under, on `certs/a`.
fn lease(turn: &Turn) -> Lease<'_> {
	Lease { service: "certs", run: "a", node: "tyo", fence: turn.fence }
}

fn workflows(entries: Vec<(&str, Workflow)>) -> HashMap<String, Workflow> {
	entries.into_iter().map(|(name, code)| (name.to_owned(), code)).collect()
}

fn counter() -> Arc<AtomicUsize> {
	Arc::new(AtomicUsize::new(0))
}

#[test]
fn a_retry_waits_as_its_backoff_says() {
	let config =
		|backoff| StepConfig { retries: Retries { limit: 5, delay_ms: 100, backoff }, timeout_ms: 1 };
	let delays = |backoff| (1..=4).map(|n| config(backoff).delay_after(n)).collect::<Vec<_>>();
	assert_eq!(delays(Backoff::Constant), [100, 100, 100, 100]);
	assert_eq!(delays(Backoff::Linear), [100, 200, 300, 400]);
	assert_eq!(delays(Backoff::Exponential), [100, 200, 400, 800]);
	assert_eq!(StepConfig::default().retries.limit, 5);
	assert_eq!(StepConfig::default().timeout_ms, 600_000);
}

#[tokio::test]
async fn a_run_is_a_task_and_a_replay_runs_a_done_step_once() {
	let Some(world) = world().await else { return };
	let issued = counter();
	let kept = counter();
	let code = workflows(vec![(
		"issue",
		workflow({
			let (issued, kept) = (issued.clone(), kept.clone());
			move |step| {
				let (issued, kept) = (issued.clone(), kept.clone());
				async move {
					let order = step
						.run("order", json!({}), || async {
							issued.fetch_add(1, Ordering::SeqCst);
							Ok(json!({ "order": "o-1" }))
						})
						.await?;
					step.sleep("propagate", 3_600_000).await?;
					step
						.run("keep", json!({}), || async {
							kept.fetch_add(1, Ordering::SeqCst);
							Ok(json!({ "kept": order["order"] }))
						})
						.await
				}
			}
		}),
	)]);
	world.open("a", "issue").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(4).await;
	assert_eq!(world.store.tick().await.unwrap().len(), 1);

	let first = node.work(&code).await;
	assert_eq!(first[0].1, Err(Halt::EndTurn));
	assert_eq!(world.state_of("a").await, "sleeping");
	assert!(world.view("a").await["lease"].is_null(), "a sleeping run holds no node");
	assert!(world.store.tick().await.unwrap().is_empty(), "nothing is due");

	world.due().await;
	assert_eq!(world.store.tick().await.unwrap().len(), 1);
	let second = node.work(&code).await;
	assert_eq!(second[0].1, Ok(json!({ "kept": "o-1" })));
	assert_eq!((issued.load(Ordering::SeqCst), kept.load(Ordering::SeqCst)), (1, 1));

	let view = world.view("a").await;
	assert_eq!(view["run"]["status"], "done");
	assert_eq!(view["run"]["fence"], 2);
	let task = world.store.get("certs", "a").await.unwrap().unwrap();
	assert_eq!(task.task.record.state, State::Done);
	assert!(task.task.record.started_at.is_some() && task.task.record.finished_at.is_some());
	let events = world.store.view("certs", "a").await.unwrap().unwrap().events;
	let stages: Vec<_> = events.iter().map(|event| event.stage.as_str()).collect();
	for stage in ["queued", "running", "lease", "step.order", "step.propagate", "step.keep", "done"] {
		assert!(stages.contains(&stage), "{stage} missing from {stages:?}");
	}
}

#[tokio::test]
async fn opening_a_run_again_is_the_same_run_and_another_with_its_id_is_refused() {
	let Some(world) = world().await else { return };
	world.open("a", "issue").await;
	let body = json!({ "workflow": "issue", "params": { "domain": "canmi.app" } });
	let (status, _) = call(&world.router, "PUT", "/v1/runs/certs/a", &body).await;
	assert_eq!(status, StatusCode::OK);
	let other = json!({ "workflow": "issue", "params": { "domain": "other.app" } });
	let (status, answer) = call(&world.router, "PUT", "/v1/runs/certs/a", &other).await;
	assert_eq!(status, StatusCode::CONFLICT);
	assert_eq!(answer["code"], "run_exists_differently");
	let local = json!({ "workflow": "issue", "class": "local" });
	assert_eq!(
		call(&world.router, "PUT", "/v1/runs/certs/b", &local).await.0,
		StatusCode::BAD_REQUEST
	);
	assert_eq!(
		call(&world.router, "GET", "/v1/runs/certs/none", &json!({})).await.0,
		StatusCode::NOT_FOUND
	);
}

#[tokio::test]
async fn a_node_lost_mid_step_loses_nothing_and_its_late_report_is_refused() {
	let Some(world) = world().await else { return };
	let ran = counter();
	let code = workflows(vec![(
		"issue",
		workflow({
			let ran = ran.clone();
			move |step| {
				let ran = ran.clone();
				async move {
					step
						.run("order", json!({}), || async {
							ran.fetch_add(1, Ordering::SeqCst);
							Ok(json!(7))
						})
						.await
				}
			}
		}),
	)]);
	world.open("a", "issue").await;
	let lost = Executor::new(&world.router, "tyo").dying_at("order");
	let survivor = Executor::new(&world.router, "sin");
	lost.poll(1).await;
	world.store.tick().await.unwrap();
	let turn = lost.poll(1).await.remove(0);
	assert_eq!(turn.fence, 1);
	assert_eq!(lost.run_turn(&code["issue"], turn.clone()).await, Err(Halt::Died));

	// Its lease runs out; the other node is given the run under the next number.
	world.lapse().await;
	survivor.poll(1).await;
	assert_eq!(world.store.tick().await.unwrap()[0].node, "sin");
	let again = survivor.poll(1).await.remove(0);
	assert_eq!(again.fence, 2);

	// The node that came back is refused, whatever it reports.
	let late = json!({ "node": "tyo", "fence": 1, "attempt": 1, "outcome": "success", "result": 1 });
	let (status, answer) = call(&world.router, "PUT", "/v1/runs/certs/a/steps/order", &late).await;
	assert_eq!(status, StatusCode::CONFLICT);
	assert_eq!(answer["code"], "stale_lease");
	assert_eq!(lost.run_turn(&code["issue"], turn).await, Err(Halt::Stale));

	assert_eq!(survivor.run_turn(&code["issue"], again).await, Ok(json!(7)));
	let view = world.view("a").await;
	let step = &view["steps"][0];
	// A lost holder is not an attempt, but it is a start.
	assert_eq!((step["attempt"].as_i64(), step["starts"].as_i64()), (Some(1), Some(2)));
	assert_eq!(view["run"]["status"], "done");
}

#[tokio::test]
async fn a_failed_step_is_retried_after_its_backoff_and_stops_at_its_limit() {
	let Some(world) = world().await else { return };
	let tries = counter();
	let code = workflows(vec![(
		"flaky",
		workflow({
			let tries = tries.clone();
			move |step| {
				let tries = tries.clone();
				async move {
					let config = json!({ "retries": { "limit": 2, "delay_ms": 50, "backoff": "constant" } });
					step
						.run("call", config, || async {
							if tries.fetch_add(1, Ordering::SeqCst) < 2 {
								Err(StepError::retry("503"))
							} else {
								Ok(json!("fine"))
							}
						})
						.await
				}
			}
		}),
	)]);
	world.open("a", "flaky").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(4).await;
	for expected in ["sleeping", "sleeping", "done"] {
		world.due().await;
		world.store.tick().await.unwrap();
		node.work(&code).await;
		assert_eq!(world.state_of("a").await, expected);
	}
	assert_eq!(tries.load(Ordering::SeqCst), 3);
	assert_eq!(world.view("a").await["steps"][0]["attempt"], 3);

	// Limit 1: the second failure is the last.
	world.open("b", "capped").await;
	let capped = workflows(vec![(
		"capped",
		workflow(|step| async move {
			let config = json!({ "retries": { "limit": 1, "delay_ms": 1, "backoff": "constant" } });
			step.run("call", config, || async { Err(StepError::retry("nope")) }).await
		}),
	)]);
	for _ in 0..2 {
		world.due().await;
		world.store.tick().await.unwrap();
		node.work(&capped).await;
	}
	let view = world.view("b").await;
	assert_eq!(
		(view["run"]["status"].as_str(), view["steps"][0]["status"].as_str()),
		(Some("failed"), Some("failed"))
	);
	assert_eq!(
		world.store.get("certs", "b").await.unwrap().unwrap().task.record.state,
		State::Failed
	);
}

#[tokio::test]
async fn a_fatal_error_is_not_retried() {
	let Some(world) = world().await else { return };
	let code = workflows(vec![(
		"w",
		workflow(|step| async move {
			step.run("call", json!({}), || async { Err(StepError::fatal("bad input")) }).await
		}),
	)]);
	world.open("a", "w").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(1).await;
	world.store.tick().await.unwrap();
	let ended = node.work(&code).await;
	assert_eq!(ended[0].1, Err(Halt::Step("bad input".into())));
	assert_eq!(world.state_of("a").await, "failed");
	assert_eq!(world.view("a").await["steps"][0]["attempt"], 1);
}

#[tokio::test]
async fn a_step_that_keeps_losing_its_holder_fails_at_twenty_starts() {
	let Some(world) = world().await else { return };
	world.open("a", "w").await;
	let node = Executor::new(&world.router, "tyo");
	let config = StepConfig::default();
	let mut last = None;
	for start in 1..=21 {
		node.poll(1).await;
		world.lapse().await;
		world.store.tick().await.unwrap();
		let turn = node.poll(1).await.remove(0);
		last =
			Some(world.store.begin_attempt(lease(&turn), "kills-the-isolate", &config).await.unwrap());
		if start <= MAX_STARTS {
			assert!(matches!(last, Some(Ok(Begun::Started { attempt: 1, .. }))), "{start}: {last:?}");
		}
	}
	assert!(matches!(last, Some(Ok(Begun::Failed(_)))), "{last:?}");
	assert_eq!(world.view("a").await["steps"][0]["status"], "failed");
}

#[tokio::test]
async fn a_step_past_its_deadline_is_a_failed_attempt() {
	let Some(world) = world().await else { return };
	world.open("a", "w").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(1).await;
	world.store.tick().await.unwrap();
	let turn = node.poll(1).await.remove(0);
	let config = StepConfig { timeout_ms: 1000, ..StepConfig::default() };
	world.store.begin_attempt(lease(&turn), "slow", &config).await.unwrap().unwrap();
	world.store.exec("UPDATE steps SET deadline = 1").await;
	world.lapse().await;
	world.store.tick().await.unwrap();
	let turn = node.poll(1).await.remove(0);
	let begun = world.store.begin_attempt(lease(&turn), "slow", &config).await;
	assert!(matches!(begun, Ok(Ok(Begun::Started { attempt: 2, .. }))), "{begun:?}");
}

#[tokio::test]
async fn a_result_past_one_mebibyte_fails_the_step() {
	let Some(world) = world().await else { return };
	world.open("a", "w").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(1).await;
	world.store.tick().await.unwrap();
	let turn = node.poll(1).await.remove(0);
	let config = StepConfig::default();
	for (name, size, fits) in [("whole", MAX_RESULT - 2, true), ("over", MAX_RESULT - 1, false)] {
		world.store.begin_attempt(lease(&turn), name, &config).await.unwrap().unwrap();
		let result = Some(Value::String("x".repeat(size)));
		let finished = world
			.store
			.finish_step(lease(&turn), name, 1, Outcome::Success, result, None)
			.await
			.unwrap()
			.unwrap();
		assert_eq!(finished, if fits { Finished::Continue } else { Finished::TooLarge }, "{name}");
	}
	let view = world.view("a").await;
	let status = |name: &str| {
		view["steps"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap()["status"].clone()
	};
	assert_eq!((status("whole"), status("over")), (json!("done"), json!("failed")));
}

#[tokio::test]
async fn a_result_reported_twice_is_one_result() {
	let Some(world) = world().await else { return };
	world.open("a", "w").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(1).await;
	world.store.tick().await.unwrap();
	let turn = node.poll(1).await.remove(0);
	let config = StepConfig::default();
	world.store.begin_attempt(lease(&turn), "s", &config).await.unwrap().unwrap();
	for _ in 0..2 {
		let done = world
			.store
			.finish_step(lease(&turn), "s", 1, Outcome::Success, Some(json!(1)), None)
			.await
			.unwrap();
		assert_eq!(done, Ok(Finished::Continue));
	}
	let again = world.store.begin_attempt(lease(&turn), "s", &config).await;
	assert!(matches!(again, Ok(Ok(Begun::Done(_)))));
}

#[tokio::test]
async fn a_replay_that_disagrees_with_the_record_is_refused() {
	let Some(world) = world().await else { return };
	world.open("a", "w").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(1).await;
	world.store.tick().await.unwrap();
	let turn = node.poll(1).await.remove(0);
	let wake = nanos(Timestamp::now()) + 3_600_000_000_000;
	world.store.sleep(lease(&turn), "s", wake).await.unwrap().unwrap();
	node.poll(1).await;
	world.due().await;
	world.store.tick().await.unwrap();
	let turn = node.poll(1).await.remove(0);
	let begun = world.store.begin_attempt(lease(&turn), "s", &StepConfig::default()).await;
	assert!(matches!(begun, Ok(Err(Refused::Diverged(_)))), "{begun:?}");
}

#[tokio::test]
async fn an_event_is_waited_for_with_nothing_held_and_taken_once() {
	let Some(world) = world().await else { return };
	let code = workflows(vec![(
		"approve",
		workflow(|step| async move {
			let first = step.wait_for_event("first", "approved", 60_000).await?;
			let second = step.wait_for_event("second", "approved", 60_000).await?;
			Ok(json!([first["payload"], second["payload"]]))
		}),
	)]);
	world.open("a", "approve").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(1).await;
	world.store.tick().await.unwrap();
	assert_eq!(node.work(&code).await[0].1, Err(Halt::EndTurn));
	assert_eq!(world.state_of("a").await, "waiting");
	assert!(world.view("a").await["lease"].is_null());

	let send = |key: &str, payload: i64| {
		let router = world.router.clone();
		let body = json!({ "key": key, "type": "approved", "payload": payload });
		async move { call(&router, "POST", "/v1/runs/certs/a/events", &body).await }
	};
	// An event of another type wakes nothing; the same key twice is one event.
	let other = json!({ "key": "x", "type": "other", "payload": 0 });
	call(&world.router, "POST", "/v1/runs/certs/a/events", &other).await;
	assert_eq!(world.state_of("a").await, "waiting");
	let (_, answer) = send("k1", 1).await;
	assert_eq!(answer["data"]["new"], true);
	assert_eq!(send("k1", 1).await.1["data"]["new"], false);
	assert_eq!(world.state_of("a").await, "ready");

	// The second is sent before the run reaches it, and is waiting there.
	send("k2", 2).await;
	world.store.tick().await.unwrap();
	assert_eq!(node.work(&code).await[0].1, Ok(json!([1, 2])));
	assert_eq!(
		call(&world.router, "POST", "/v1/runs/certs/none/events", &json!({"key": "k", "type": "t"}))
			.await
			.0,
		StatusCode::NOT_FOUND
	);
}

#[tokio::test]
async fn a_wait_that_times_out_fails_its_step() {
	let Some(world) = world().await else { return };
	let code = workflows(vec![(
		"approve",
		workflow(|step| async move {
			match step.wait_for_event("ask", "approved", 60_000).await {
				Err(Halt::Step(error)) => Ok(json!({ "gave_up": error })),
				other => other,
			}
		}),
	)]);
	world.open("a", "approve").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(1).await;
	world.store.tick().await.unwrap();
	node.work(&code).await;
	world.due().await;
	world.store.tick().await.unwrap();
	let ended = node.work(&code).await;
	assert!(ended[0].1.as_ref().unwrap()["gave_up"].as_str().unwrap().contains("timed out"));
}

#[tokio::test(flavor = "multi_thread")]
async fn two_schedulers_ticking_at_once_lease_a_run_once() {
	let Some(world) = world().await else { return };
	world.open("a", "w").await;
	let (one, other) = (Executor::new(&world.router, "tyo"), Executor::new(&world.router, "sin"));
	one.poll(4).await;
	other.poll(4).await;
	let (left, right) = tokio::join!(world.store.tick(), world.store.tick());
	assert_eq!(left.unwrap().len() + right.unwrap().len(), 1);
	let view = world.view("a").await;
	assert_eq!(view["run"]["fence"], 1);
	assert!(view["lease"]["holder"].is_string());
}

#[tokio::test]
async fn runs_go_to_the_least_loaded_node_and_never_past_its_capacity() {
	let Some(world) = world().await else { return };
	for id in ["a", "b", "c"] {
		world.open(id, "w").await;
	}
	let (one, other) = (Executor::new(&world.router, "sin"), Executor::new(&world.router, "tyo"));
	one.poll(1).await;
	other.poll(2).await;
	let granted = world.store.tick().await.unwrap();
	let mut by_node: Vec<_> = granted.iter().map(|grant| grant.node.as_str()).collect();
	by_node.sort_unstable();
	assert_eq!(by_node, ["sin", "tyo", "tyo"]);
	// All taken: nothing more to give, and nothing is leased twice.
	world.open("d", "w").await;
	assert!(world.store.tick().await.unwrap().is_empty());
}

#[tokio::test]
async fn an_executor_not_heard_from_is_given_nothing() {
	let Some(world) = world().await else { return };
	world.open("a", "w").await;
	Executor::new(&world.router, "tyo").poll(1).await;
	world.store.exec("UPDATE executors SET seen_at = 0").await;
	assert!(world.store.tick().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_turn_is_pushed_and_found_by_polling_alike() {
	let Some(world) = world().await else { return };
	world.open("a", "w").await;
	let pushed = Executor::pushed(&world.router, "tyo").await;
	pushed.poll(1).await;
	let http = crate::scheduler::client();
	assert_eq!(crate::scheduler::round(&world.store, &http).await.unwrap(), 1);
	let turn = pushed.pushed_turn().await.expect("the ledger pushed the turn");
	assert_eq!((turn.run.as_str(), turn.fence), ("a", 1));
	assert_eq!(pushed.poll(1).await, vec![turn], "polling finds the same turn");

	// A push to nobody costs a log line, never the lease.
	world.open("b", "w").await;
	let mut gone = Executor::new(&world.router, "sin");
	gone.push_url = Some("http://push.invalid/turns".into());
	gone.poll(1).await;
	assert_eq!(crate::scheduler::round(&world.store, &http).await.unwrap(), 1);
	assert_eq!(gone.poll(1).await.len(), 1);
}

#[tokio::test]
async fn a_renewal_extends_a_lease_only_for_its_holder() {
	let Some(world) = world().await else { return };
	world.open("a", "w").await;
	let node = Executor::new(&world.router, "tyo");
	node.poll(1).await;
	world.store.tick().await.unwrap();
	let turn = node.poll(1).await.remove(0);
	world.store.exec("UPDATE leases SET expires_at = expires_at - 20000000000").await;
	let before = world.view("a").await["lease"]["expires_at"].clone();
	let renewed = world.store.renew(lease(&turn)).await.unwrap().unwrap();
	assert!(renewed > serde_json::from_value::<Timestamp>(before).unwrap());
	assert!(world.store.renew(Lease { node: "sin", ..lease(&turn) }).await.unwrap().is_none());
	assert!(
		world.store.renew(Lease { fence: turn.fence + 1, ..lease(&turn) }).await.unwrap().is_none()
	);
	let (status, _) =
		call(&world.router, "PUT", "/v1/runs/certs/a/lease", &json!({ "node": "tyo", "fence": 9 }))
			.await;
	assert_eq!(status, StatusCode::CONFLICT);
}

#[tokio::test]
async fn the_gate_is_open_only_where_patroni_says_the_node_leads() {
	let serve = |status: StatusCode| async move {
		let app = Router::new().route("/primary", axum::routing::get(move || async move { status }));
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let url = format!("http://{}/primary", listener.local_addr().unwrap());
		tokio::spawn(async move { axum::serve(listener, app).await });
		url
	};
	let http = crate::scheduler::client();
	assert!(crate::scheduler::leads(&http, &serve(StatusCode::OK).await).await);
	assert!(!crate::scheduler::leads(&http, &serve(StatusCode::SERVICE_UNAVAILABLE).await).await);
	assert!(!crate::scheduler::leads(&http, "http://gate.invalid/primary").await);
}
