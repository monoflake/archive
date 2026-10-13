//! What every node's relay reads, and an operator writes, over the tailnet. See
//! spec/architecture/registry.md. The routes outside `/v1` are the ones every relay in production
//! reads, kept as they are until none does; `/v1` is in v1.rs.

use crate::node;
use crate::store::Store;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Registry {
	pub store: Store,
	/// What a write's bearer has to be; empty, every write is refused.
	pub token: String,
}

pub type Shared = Arc<Registry>;

/// A page of `GET /changes`, by default and at most.
const DEFAULT_LIMIT: usize = 100;
const MAX_LIMIT: usize = 500;

pub fn routes(registry: Shared) -> Router {
	Router::new()
		.route("/health", get(health))
		.route("/nodes", get(nodes))
		.route("/nodes/{name}", axum::routing::put(put).delete(delete))
		.route("/changes", get(changes))
		.merge(v1::routes())
		.fallback(|| async { response::failure(StatusCode::NOT_FOUND, "no_such_route") })
		.with_state(registry)
}

/// Healthy while the database answers, since nothing is read or written without it.
async fn health(State(registry): State<Shared>) -> Response {
	match registry.store.answers().await {
		Ok(()) => response::success(StatusCode::OK, ()),
		Err(error) => {
			response::failure_with(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable", error)
		}
	}
}

async fn nodes(State(registry): State<Shared>) -> Response {
	match registry.store.nodes().await {
		Ok(nodes) => response::success(StatusCode::OK, nodes),
		Err(error) => {
			response::failure_with(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error)
		}
	}
}

/// A write's refusal, if it has one: closed without a token, and unauthorized without its bearer,
/// compared in constant time so the answer's timing says nothing of how much of it matched.
fn refused(registry: &Registry, headers: &HeaderMap) -> Option<Response> {
	if registry.token.is_empty() {
		return Some(response::failure_with(
			StatusCode::SERVICE_UNAVAILABLE,
			"service_unavailable",
			"Writes are closed: this registry has no token set",
		));
	}
	let given = headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.unwrap_or_default();
	let token = registry.token.as_bytes();
	let admitted = given.len() == token.len()
		&& given.bytes().zip(token).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0;
	(!admitted).then(|| response::failure(StatusCode::UNAUTHORIZED, "invalid_token"))
}

fn invalid_name() -> Response {
	response::failure_with(
		StatusCode::BAD_REQUEST,
		"invalid_name",
		"A node's name is three lowercase letters, an IATA code",
	)
}

/// Who made a change: whoever the operator's task or the console says it is, never empty.
fn by_is_valid(by: &str) -> bool {
	!by.trim().is_empty()
}

#[derive(serde::Deserialize)]
struct Put {
	by: String,
	node: Value,
}

/// Sets a node, appending the change; a record equal to the one kept writes nothing.
async fn put(
	Path(name): Path<String>,
	State(registry): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Put>, JsonRejection>,
) -> Response {
	if let Some(refusal) = refused(&registry, &headers) {
		return refusal;
	}
	if !node::name_is_valid(&name) {
		return invalid_name();
	}
	let Ok(axum::Json(Put { by, node })) = body else {
		return response::failure(StatusCode::BAD_REQUEST, "invalid_body");
	};
	if !by_is_valid(&by) {
		return response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", "`by` names who");
	}
	let record = match node::record(node) {
		Ok(record) => record,
		Err(message) => {
			return response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message);
		}
	};
	match registry.store.put(&name, &by, Value::Object(record)).await {
		Ok(change) => response::success(StatusCode::OK, changed(change)),
		Err(error) => {
			response::failure_with(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error)
		}
	}
}

/// A write's answer: the change it appended, or `{}` when there was nothing to change.
fn changed(change: Option<i64>) -> Value {
	match change {
		Some(change) => json!({ "change": change }),
		None => json!({}),
	}
}

#[derive(serde::Deserialize)]
struct Delete {
	by: String,
}

/// Deletes a node, appending the change; a node that is not there writes nothing.
async fn delete(
	Path(name): Path<String>,
	State(registry): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Delete>, JsonRejection>,
) -> Response {
	if let Some(refusal) = refused(&registry, &headers) {
		return refusal;
	}
	if !node::name_is_valid(&name) {
		return invalid_name();
	}
	let Ok(axum::Json(Delete { by })) = body else {
		return response::failure(StatusCode::BAD_REQUEST, "invalid_body");
	};
	if !by_is_valid(&by) {
		return response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", "`by` names who");
	}
	match registry.store.delete(&name, &by).await {
		Ok(change) => response::success(StatusCode::OK, changed(change)),
		Err(error) => {
			response::failure_with(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error)
		}
	}
}

#[derive(serde::Deserialize)]
struct ChangesAsked {
	after: Option<String>,
	limit: Option<String>,
}

/// `after` and `limit` that do not parse fall back to their defaults, and a `limit` out of range is
/// brought into it, rather than failing a read that is otherwise well formed.
fn page_asked(asked: &ChangesAsked) -> (i64, usize) {
	let after = asked.after.as_deref().and_then(|text| text.parse().ok()).unwrap_or(0);
	let limit = asked
		.limit
		.as_deref()
		.and_then(|text| text.parse::<usize>().ok())
		.unwrap_or(DEFAULT_LIMIT)
		.clamp(1, MAX_LIMIT);
	(after, limit)
}

/// Oldest first, every change numbered after `after`, a page at a time.
async fn changes(State(registry): State<Shared>, Query(asked): Query<ChangesAsked>) -> Response {
	let (after, limit) = page_asked(&asked);
	match registry.store.changes(after, limit).await {
		Ok(changes) => response::success(StatusCode::OK, json!({ "changes": changes })),
		Err(error) => {
			response::failure_with(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error)
		}
	}
}

mod v1;

#[cfg(test)]
mod tests;
