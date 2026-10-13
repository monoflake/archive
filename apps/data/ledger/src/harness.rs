//! A test executor: the executors' half of `runs_api`, in the shape Workflows' `step` has, driving
//! the coordinator through its HTTP routes as a node running workerd will. A workflow is a closure
//! over `Steps`; a turn replays it from the top. See spec/architecture/ledger.md, "A run, and the
//! node that replays it".

use crate::store::runs::Turn;
use axum::Router;
use axum::body::Body;
use axum::http::{Request, StatusCode};
use http_body_util::BodyExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::mpsc;
use tower::ServiceExt;

/// Why a turn stopped before the workflow returned.
#[derive(Debug, PartialEq)]
pub enum Halt {
	/// A sleep, a wait or a retry released the lease: the next turn picks the run up.
	EndTurn,
	/// The lease is not this node's now: abandon the turn.
	Stale,
	/// The node was lost at this step, its result never reported.
	Died,
	/// A step failed for good, which the workflow may catch.
	Step(String),
}

/// What a step's callback returns when it fails.
pub struct StepError {
	pub message: String,
	pub fatal: bool,
}

impl StepError {
	pub fn retry(message: &str) -> Self {
		Self { message: message.to_owned(), fatal: false }
	}

	pub fn fatal(message: &str) -> Self {
		Self { message: message.to_owned(), fatal: true }
	}
}

pub type Replayed = Pin<Box<dyn Future<Output = Result<Value, Halt>> + Send>>;
pub type Workflow = Arc<dyn Fn(Steps) -> Replayed + Send + Sync>;

pub fn workflow<F, Fut>(code: F) -> Workflow
where
	F: Fn(Steps) -> Fut + Send + Sync + 'static,
	Fut: Future<Output = Result<Value, Halt>> + Send + 'static,
{
	Arc::new(move |steps| Box::pin(code(steps)))
}

/// One node's side of the API.
#[derive(Clone)]
pub struct Executor {
	pub node: String,
	router: Router,
	/// The step at which this node is lost, after it began and before it reported.
	die_at: Option<String>,
	/// Where a pushed turn arrives, when this node gave a `push_url`.
	pushed: Option<Arc<tokio::sync::Mutex<mpsc::UnboundedReceiver<Turn>>>>,
	pub push_url: Option<String>,
}

pub async fn call(router: &Router, method: &str, path: &str, body: &Value) -> (StatusCode, Value) {
	let request = Request::builder()
		.method(method)
		.uri(path)
		.header("content-type", "application/json")
		.body(Body::from(serde_json::to_vec(body).unwrap()))
		.unwrap();
	let answer = router.clone().oneshot(request).await.unwrap();
	let status = answer.status();
	let bytes = answer.into_body().collect().await.unwrap().to_bytes();
	(status, serde_json::from_slice(&bytes).unwrap_or(Value::Null))
}

impl Executor {
	pub fn new(router: &Router, node: &str) -> Self {
		Self {
			node: node.to_owned(),
			router: router.clone(),
			die_at: None,
			pushed: None,
			push_url: None,
		}
	}

	/// A node that tells the ledger where to push a turn: a server of its own on 127.0.0.1.
	pub async fn pushed(router: &Router, node: &str) -> Self {
		let (send, receive) = mpsc::unbounded_channel::<Turn>();
		let app = Router::new().route(
			"/turns",
			axum::routing::post(move |axum::Json(turn): axum::Json<Turn>| async move {
				let _ = send.send(turn);
				StatusCode::OK
			}),
		);
		let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
		let url = format!("http://{}/turns", listener.local_addr().unwrap());
		tokio::spawn(async move { axum::serve(listener, app).await });
		Self {
			pushed: Some(Arc::new(tokio::sync::Mutex::new(receive))),
			push_url: Some(url),
			..Self::new(router, node)
		}
	}

	pub fn dying_at(mut self, step: &str) -> Self {
		self.die_at = Some(step.to_owned());
		self
	}

	/// The heartbeat, and the turns this node holds.
	pub async fn poll(&self, capacity: i32) -> Vec<Turn> {
		let body = json!({ "capacity": capacity, "push_url": self.push_url });
		let (status, answer) =
			call(&self.router, "POST", &format!("/v1/executors/{}/polls", self.node), &body).await;
		assert_eq!(status, StatusCode::OK, "{answer}");
		serde_json::from_value(answer["data"]["turns"].clone()).unwrap()
	}

	/// The turn the ledger pushed, waiting a little for it.
	pub async fn pushed_turn(&self) -> Option<Turn> {
		let mut receive = self.pushed.as_ref()?.lock().await;
		tokio::time::timeout(std::time::Duration::from_secs(2), receive.recv()).await.ok().flatten()
	}

	/// Replays `turn` through `code`, and tells the ledger how it ended. Answers why it stopped.
	pub async fn run_turn(&self, code: &Workflow, turn: Turn) -> Result<Value, Halt> {
		let steps = Steps { executor: self.clone(), turn: turn.clone() };
		let ended = code(steps).await;
		let path = format!("/v1/runs/{}/{}/outcome", turn.service, turn.run);
		let holder = json!({ "node": self.node, "fence": turn.fence });
		let end = match &ended {
			Ok(output) => Some(json!({ "outcome": "done", "output": output })),
			Err(Halt::Step(error)) => Some(json!({ "outcome": "failed", "error": error })),
			Err(_) => None,
		};
		if let Some(end) = end {
			let mut body = holder;
			body.as_object_mut().unwrap().extend(end.as_object().unwrap().clone());
			let (status, answer) = call(&self.router, "PUT", &path, &body).await;
			assert_eq!(status, StatusCode::OK, "{answer}");
		}
		ended
	}

	/// Polls and replays every turn this node holds, by the workflow named in each.
	pub async fn work(&self, code: &HashMap<String, Workflow>) -> Vec<(String, Result<Value, Halt>)> {
		let mut ended = Vec::new();
		for turn in self.poll(8).await {
			let run = turn.run.clone();
			ended.push((run, self.run_turn(&code[&turn.workflow], turn).await));
		}
		ended
	}
}

/// What a workflow is given: Workflows' `step`.
pub struct Steps {
	executor: Executor,
	pub turn: Turn,
}

impl Steps {
	fn path(&self, tail: &str) -> String {
		format!("/v1/runs/{}/{}{tail}", self.turn.service, self.turn.run)
	}

	fn holder(&self) -> Value {
		json!({ "node": self.executor.node, "fence": self.turn.fence })
	}

	async fn call(&self, method: &str, tail: &str, extra: Value) -> (StatusCode, Value) {
		let mut body = self.holder();
		body.as_object_mut().unwrap().extend(extra.as_object().unwrap().clone());
		call(&self.executor.router, method, &self.path(tail), &body).await
	}

	fn halted(status: StatusCode, answer: &Value) -> Halt {
		assert_eq!(status, StatusCode::CONFLICT, "{answer}");
		Halt::Stale
	}

	/// `step.do`: answered from the turn when done, else begun, run and reported.
	pub async fn run<F, Fut>(&self, name: &str, config: Value, code: F) -> Result<Value, Halt>
	where
		F: FnOnce() -> Fut,
		Fut: Future<Output = Result<Value, StepError>>,
	{
		if let Some(result) = self.turn.done.get(name) {
			return Ok(result.clone());
		}
		let (status, answer) =
			self.call("POST", &format!("/steps/{name}/attempts"), json!({ "config": config })).await;
		if status != StatusCode::OK {
			return Err(Self::halted(status, &answer));
		}
		let data = &answer["data"];
		match data["status"].as_str().unwrap() {
			"done" => return Ok(data["result"].clone()),
			"failed" => return Err(Halt::Step(data["error"].as_str().unwrap().to_owned())),
			_ => {}
		}
		let attempt = data["attempt"].as_i64().unwrap();
		if self.executor.die_at.as_deref() == Some(name) {
			return Err(Halt::Died);
		}
		let body = match code().await {
			Ok(result) => json!({ "attempt": attempt, "outcome": "success", "result": result }),
			Err(error) => json!({
				"attempt": attempt,
				"outcome": if error.fatal { "fatal" } else { "error" },
				"error": error.message,
			}),
		};
		let (status, answer) = self.call("PUT", &format!("/steps/{name}"), body.clone()).await;
		if status == StatusCode::PAYLOAD_TOO_LARGE {
			return Err(Halt::Step("the result is larger than 1 MiB".to_owned()));
		}
		if status != StatusCode::OK {
			return Err(Self::halted(status, &answer));
		}
		let data = &answer["data"];
		match data["next"].as_str().unwrap() {
			"continue" => Ok(body["result"].clone()),
			"retry" => Err(Halt::EndTurn),
			_ => Err(Halt::Step(data["error"].as_str().unwrap().to_owned())),
		}
	}

	/// `step.sleepUntil`: `wake_at` is an instant, fixed by the first turn that reaches it.
	pub async fn sleep_until(&self, name: &str, wake_at: jiff::Timestamp) -> Result<Value, Halt> {
		if self.turn.done.contains_key(name) {
			return Ok(Value::Null);
		}
		let (status, answer) =
			self.call("POST", "/sleeps", json!({ "name": name, "wake_at": wake_at })).await;
		if status != StatusCode::OK {
			return Err(Self::halted(status, &answer));
		}
		match answer["data"]["status"].as_str().unwrap() {
			"done" => Ok(Value::Null),
			_ => Err(Halt::EndTurn),
		}
	}

	/// `step.sleep`: a duration from now.
	pub async fn sleep(&self, name: &str, milliseconds: i64) -> Result<Value, Halt> {
		let wake_at = jiff::Timestamp::now() + jiff::SignedDuration::from_millis(milliseconds);
		self.sleep_until(name, wake_at).await
	}

	/// `step.waitForEvent`.
	pub async fn wait_for_event(
		&self,
		name: &str,
		event_type: &str,
		timeout_ms: u64,
	) -> Result<Value, Halt> {
		if let Some(event) = self.turn.done.get(name) {
			return Ok(event.clone());
		}
		let body = json!({ "name": name, "type": event_type, "timeout_ms": timeout_ms });
		let (status, answer) = self.call("POST", "/waits", body).await;
		if status != StatusCode::OK {
			return Err(Self::halted(status, &answer));
		}
		let data = &answer["data"];
		match data["status"].as_str().unwrap() {
			"done" => Ok(data["event"].clone()),
			"failed" => Err(Halt::Step(data["error"].as_str().unwrap().to_owned())),
			_ => Err(Halt::EndTurn),
		}
	}
}
