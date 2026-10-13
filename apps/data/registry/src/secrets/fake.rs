//! A secret service on 127.0.0.1 that keeps every request it is sent, answering as the real one
//! does a scope token's read and write -- or refusing, as told.

use axum::Router;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};
use std::collections::BTreeMap;
use std::sync::{Arc, Mutex};

/// One request as the fake took it.
#[derive(Debug, Clone, PartialEq)]
pub struct Asked {
	pub method: &'static str,
	pub path: String,
	pub scope: Option<String>,
	pub bearer: String,
	pub body: Value,
}

#[derive(Default)]
pub struct Held {
	pub asked: Vec<Asked>,
	/// Each name's revision, as its writes made it.
	pub revisions: BTreeMap<String, i64>,
	/// What every request is refused with, when set: its status and code.
	pub refusing: Option<(u16, &'static str)>,
}

pub type Fake = Arc<Mutex<Held>>;

#[derive(serde::Deserialize)]
struct Scoped {
	scope: Option<String>,
}

fn bearer(headers: &HeaderMap) -> String {
	headers.get("authorization").and_then(|value| value.to_str().ok()).unwrap_or_default().to_owned()
}

fn refused(held: &Held) -> Option<Response> {
	let (status, code) = held.refusing?;
	let status = StatusCode::from_u16(status).unwrap();
	Some(response::failure(status, code))
}

/// The fake, listening, and the base its client asks.
pub async fn serve() -> (String, Fake) {
	let fake: Fake = Arc::default();
	let read = |Path((app, name)): Path<(String, String)>,
	            Query(scoped): Query<Scoped>,
	            State(fake): State<Fake>,
	            headers: HeaderMap| async move {
		let mut held = fake.lock().unwrap();
		let path = format!("/v1/secrets/{app}/{name}");
		held.asked.push(Asked {
			method: "GET",
			path,
			scope: scoped.scope,
			bearer: bearer(&headers),
			body: Value::Null,
		});
		if let Some(refusal) = refused(&held) {
			return refusal;
		}
		match held.revisions.get(&name) {
			Some(revision) => {
				response::success(StatusCode::OK, json!({ "name": name, "revision": revision }))
			}
			None => response::failure(StatusCode::NOT_FOUND, "no_such_resource"),
		}
	};
	let write = |Path((app, name)): Path<(String, String)>,
	             Query(scoped): Query<Scoped>,
	             State(fake): State<Fake>,
	             headers: HeaderMap,
	             axum::Json(body): axum::Json<Value>| async move {
		let mut held = fake.lock().unwrap();
		let path = format!("/v1/secrets/{app}/{name}");
		let asked = Asked {
			method: "PUT",
			path,
			scope: scoped.scope,
			bearer: bearer(&headers),
			body: body.clone(),
		};
		held.asked.push(asked);
		if let Some(refusal) = refused(&held) {
			return refusal;
		}
		let kept = held.revisions.get(&name).copied().unwrap_or(0);
		if body["revision"] != json!(kept) {
			return response::failure(StatusCode::CONFLICT, "invalid_revision");
		}
		held.revisions.insert(name, kept + 1);
		response::success(StatusCode::OK, json!({ "revision": kept + 1 }))
	};
	let router =
		Router::new().route("/v1/secrets/{app}/{name}", get(read).put(write)).with_state(fake.clone());
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	tokio::spawn(async move { axum::serve(listener, router).await });
	(format!("http://{address}"), fake)
}

/// A base nothing listens on: bound, then let go.
pub async fn unreachable() -> String {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	drop(listener);
	format!("http://{address}")
}
