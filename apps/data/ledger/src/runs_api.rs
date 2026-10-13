//! What the executors and the services that open a run reach, under `/v1`. See
//! spec/architecture/ledger.md, "A run, and the node that replays it".
//!
//! The response catalogue has no code yet for a stale lease, a replay that disagrees, a run that
//! exists differently or a result past its size; each is answered under the nearest code, with its
//! own word first in the message.

use crate::api::Shared;
use crate::store::nanos;
use crate::store::runs::{
	Begun, Created, Finished, Lease, NewRun, Outcome, Refused, Slept, StepConfig, Waited,
};
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::Response;
use axum::routing::{post, put};
use jiff::Timestamp;
use serde::Deserialize;
use serde_json::{Value, json};

pub fn routes() -> Router<Shared> {
	Router::new()
		.route("/v1/runs/{service}/{id}", put(create).get(view))
		.route("/v1/executors/{node}/polls", post(poll))
		.route("/v1/runs/{service}/{id}/lease", put(renew))
		.route("/v1/runs/{service}/{id}/steps/{name}/attempts", post(begin))
		.route("/v1/runs/{service}/{id}/steps/{name}", put(finish))
		.route("/v1/runs/{service}/{id}/sleeps", post(sleep))
		.route("/v1/runs/{service}/{id}/waits", post(wait))
		.route("/v1/runs/{service}/{id}/events", post(event))
		.route("/v1/runs/{service}/{id}/outcome", put(outcome))
}

type Body<T> = Result<axum::Json<T>, JsonRejection>;

fn invalid() -> Response {
	response::failure(StatusCode::BAD_REQUEST, "invalid_body")
}

fn unavailable(error: anyhow::Error) -> Response {
	response::failure_with(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error)
}

fn refused(refusal: Refused) -> Response {
	match refusal {
		Refused::NoSuchRun => response::failure_with(
			StatusCode::NOT_FOUND,
			"no_such_task",
			"no_such_run: no run is kept under this id",
		),
		Refused::Stale => response::failure_with(
			StatusCode::CONFLICT,
			"invalid_revision",
			"stale_lease: the run is not leased to this node at this fence",
		),
		Refused::Diverged(why) => response::failure_with(
			StatusCode::CONFLICT,
			"invalid_revision",
			format!("replay_diverged: {why}"),
		),
	}
}

/// A write that needs the lease, answered: `ok` makes the success of what it did.
fn checked<T>(
	done: anyhow::Result<Result<T, Refused>>,
	ok: impl FnOnce(T) -> Response,
) -> Response {
	match done {
		Ok(Ok(value)) => ok(value),
		Ok(Err(why)) => refused(why),
		Err(error) => unavailable(error),
	}
}

/// What a lease-holder says of itself in every write.
#[derive(Deserialize)]
struct Holder {
	node: String,
	fence: i64,
}

impl Holder {
	fn on<'a>(&'a self, service: &'a str, run: &'a str) -> Lease<'a> {
		Lease { service, run, node: &self.node, fence: self.fence }
	}
}

async fn create(
	Path((service, id)): Path<(String, String)>,
	State(store): State<Shared>,
	body: Body<NewRun>,
) -> Response {
	let Ok(axum::Json(new)) = body else { return invalid() };
	if new.workflow.is_empty() || service.is_empty() || id.is_empty() {
		return invalid();
	}
	let made = match store.create_run(&service, &id, &new).await {
		Ok(made) => made,
		Err(error) => return unavailable(error),
	};
	let status = match made {
		Created::New => StatusCode::CREATED,
		Created::Same => StatusCode::OK,
		Created::Differs => {
			return response::failure_with(
				StatusCode::CONFLICT,
				"invalid_revision",
				"run_exists_differently: this id is a run of another workflow, class or params",
			);
		}
		Created::Unsupported => return invalid(),
	};
	match store.run_view(&service, &id).await {
		Ok(Some(run)) => response::success(status, run),
		Ok(None) => refused(Refused::NoSuchRun),
		Err(error) => unavailable(error),
	}
}

async fn view(
	Path((service, id)): Path<(String, String)>,
	State(store): State<Shared>,
) -> Response {
	match store.run_view(&service, &id).await {
		Ok(Some(run)) => response::success(StatusCode::OK, run),
		Ok(None) => refused(Refused::NoSuchRun),
		Err(error) => unavailable(error),
	}
}

#[derive(Deserialize)]
struct Polled {
	capacity: i32,
	push_url: Option<String>,
}

async fn poll(
	Path(node): Path<String>,
	State(store): State<Shared>,
	body: Body<Polled>,
) -> Response {
	let Ok(axum::Json(polled)) = body else { return invalid() };
	if node.is_empty() || polled.capacity < 0 {
		return invalid();
	}
	match store.poll(&node, polled.capacity, polled.push_url.as_deref()).await {
		Ok(turns) => response::success(StatusCode::OK, json!({ "turns": turns })),
		Err(error) => unavailable(error),
	}
}

async fn renew(
	Path((service, id)): Path<(String, String)>,
	State(store): State<Shared>,
	body: Body<Holder>,
) -> Response {
	let Ok(axum::Json(holder)) = body else { return invalid() };
	match store.renew(holder.on(&service, &id)).await {
		Ok(Some(expires_at)) => response::success(StatusCode::OK, json!({ "expires_at": expires_at })),
		Ok(None) => refused(Refused::Stale),
		Err(error) => unavailable(error),
	}
}

#[derive(Deserialize)]
struct Begin {
	#[serde(flatten)]
	holder: Holder,
	#[serde(default)]
	config: StepConfig,
}

async fn begin(
	Path((service, id, name)): Path<(String, String, String)>,
	State(store): State<Shared>,
	body: Body<Begin>,
) -> Response {
	let Ok(axum::Json(begin)) = body else { return invalid() };
	if name.is_empty() || !begin.config.valid() {
		return invalid();
	}
	let done = store.begin_attempt(begin.holder.on(&service, &id), &name, &begin.config);
	checked(done.await, |begun| {
		response::success(
			StatusCode::OK,
			match begun {
				Begun::Done(result) => json!({ "status": "done", "result": result }),
				Begun::Failed(error) => json!({ "status": "failed", "error": error }),
				Begun::Started { attempt, deadline } => {
					json!({ "status": "started", "attempt": attempt, "deadline": deadline })
				}
			},
		)
	})
}

#[derive(Deserialize)]
struct Finish {
	#[serde(flatten)]
	holder: Holder,
	attempt: i32,
	outcome: Outcome,
	result: Option<Value>,
	error: Option<String>,
}

async fn finish(
	Path((service, id, name)): Path<(String, String, String)>,
	State(store): State<Shared>,
	body: Body<Finish>,
) -> Response {
	let Ok(axum::Json(finish)) = body else { return invalid() };
	let done = store.finish_step(
		finish.holder.on(&service, &id),
		&name,
		finish.attempt,
		finish.outcome,
		finish.result,
		finish.error,
	);
	checked(done.await, |finished| match finished {
		Finished::Continue => response::success(StatusCode::OK, json!({ "next": "continue" })),
		Finished::Retry { run_at } => {
			response::success(StatusCode::OK, json!({ "next": "retry", "run_at": run_at }))
		}
		Finished::Fail { error } => {
			response::success(StatusCode::OK, json!({ "next": "fail", "error": error }))
		}
		Finished::TooLarge => response::failure_with(
			StatusCode::PAYLOAD_TOO_LARGE,
			"invalid_size",
			"result_too_large: a step's result is at most 1 MiB",
		),
	})
}

#[derive(Deserialize)]
struct Sleep {
	#[serde(flatten)]
	holder: Holder,
	name: String,
	wake_at: Timestamp,
}

async fn sleep(
	Path((service, id)): Path<(String, String)>,
	State(store): State<Shared>,
	body: Body<Sleep>,
) -> Response {
	let Ok(axum::Json(sleep)) = body else { return invalid() };
	if sleep.name.is_empty() {
		return invalid();
	}
	let done = store.sleep(sleep.holder.on(&service, &id), &sleep.name, nanos(sleep.wake_at));
	checked(done.await, |slept| {
		response::success(
			StatusCode::OK,
			match slept {
				Slept::Done => json!({ "status": "done" }),
				Slept::Sleeping { wake_at } => json!({ "status": "sleeping", "wake_at": wake_at }),
			},
		)
	})
}

#[derive(Deserialize)]
struct Wait {
	#[serde(flatten)]
	holder: Holder,
	name: String,
	#[serde(rename = "type")]
	event_type: String,
	timeout_ms: u64,
}

async fn wait(
	Path((service, id)): Path<(String, String)>,
	State(store): State<Shared>,
	body: Body<Wait>,
) -> Response {
	let Ok(axum::Json(wait)) = body else { return invalid() };
	if wait.name.is_empty() || wait.event_type.is_empty() || wait.timeout_ms == 0 {
		return invalid();
	}
	let done =
		store.wait(wait.holder.on(&service, &id), &wait.name, &wait.event_type, wait.timeout_ms);
	checked(done.await, |waited| {
		response::success(
			StatusCode::OK,
			match waited {
				Waited::Done(event) => json!({ "status": "done", "event": event }),
				Waited::Failed(error) => json!({ "status": "failed", "error": error }),
				Waited::Waiting { wake_at } => json!({ "status": "waiting", "wake_at": wake_at }),
			},
		)
	})
}

#[derive(Deserialize)]
struct Event {
	key: String,
	#[serde(rename = "type")]
	event_type: String,
	#[serde(default)]
	payload: Value,
}

async fn event(
	Path((service, id)): Path<(String, String)>,
	State(store): State<Shared>,
	body: Body<Event>,
) -> Response {
	let Ok(axum::Json(event)) = body else { return invalid() };
	if event.key.is_empty() || event.event_type.is_empty() {
		return invalid();
	}
	match store.send_event(&service, &id, &event.key, &event.event_type, &event.payload).await {
		Ok(Some(new)) => response::success(StatusCode::OK, json!({ "new": new })),
		Ok(None) => refused(Refused::NoSuchRun),
		Err(error) => unavailable(error),
	}
}

#[derive(Deserialize)]
struct End {
	#[serde(flatten)]
	holder: Holder,
	outcome: Ended,
	output: Option<Value>,
	error: Option<String>,
}

#[derive(Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
enum Ended {
	Done,
	Failed,
}

async fn outcome(
	Path((service, id)): Path<(String, String)>,
	State(store): State<Shared>,
	body: Body<End>,
) -> Response {
	let Ok(axum::Json(end)) = body else { return invalid() };
	let done = store.finish_run(
		end.holder.on(&service, &id),
		end.outcome == Ended::Done,
		end.output,
		end.error,
	);
	checked(done.await, |()| response::success(StatusCode::OK, json!({})))
}

#[cfg(test)]
mod tests {
	#[test]
	fn every_code_it_answers_with_is_in_the_catalogue() {
		for code in response::codes_named(include_str!("runs_api.rs")) {
			assert!(
				response::message_of(code).is_some(),
				"`{code}` is not in the response crate's codes.json"
			);
		}
	}
}
