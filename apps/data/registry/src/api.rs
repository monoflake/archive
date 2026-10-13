//! What every node's relay and host read, and an operator writes, over the tailnet: `/health`
//! here, and the resources under `/v1` in v1.rs. See spec/architecture/registry.md.

use crate::store::Store;
use axum::Router;
use axum::extract::State;
use axum::http::header::AUTHORIZATION;
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::get;
use std::sync::Arc;

pub struct Registry {
	pub store: Store,
	/// What a write's bearer has to be; empty, every write is refused.
	pub token: String,
	/// Where a write that changed something is told as a task; none, and it is told nowhere.
	pub ledger: Option<ledger::Ledger>,
	/// Where an account identity's value is handed; none, and a write carrying one is refused.
	pub secrets: Option<crate::secrets::Secrets>,
}

pub type Shared = Arc<Registry>;

pub fn routes(registry: Shared) -> Router {
	Router::new()
		.route("/health", get(health))
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

mod v1;

#[cfg(test)]
mod tests;
