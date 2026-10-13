//! The control contract's first slice: nodes and apps as resources of a kind, and the catalog that
//! describes them, under `/v1`. Reading is open as the routes above it are, and writing takes the
//! same token. See RFC cpc2b.

use super::{Shared, by_is_valid, invalid_name, refused};
use crate::app::{self, Refusal};
use crate::kinds::{CATALOG, Kind};
use crate::node;
use crate::store::Set;
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{any, get};
use serde_json::{Map, Value, json};

pub fn routes() -> Router<Shared> {
	Router::new()
		.route("/v1/kinds", get(kinds))
		.route("/v1/nodes", get(nodes))
		.route("/v1/nodes/{name}", get(node).put(put_node))
		.route("/v1/apps", get(apps))
		.route("/v1/apps/{name}", get(app).put(put_app).patch(patch_app))
		.route("/v1/{kind}", any(no_such_kind))
		.route("/v1/{kind}/{name}", any(no_such_kind))
}

async fn kinds() -> Response {
	response::success(StatusCode::OK, json!({ "items": CATALOG }))
}

async fn no_such_kind() -> Response {
	response::failure(StatusCode::NOT_FOUND, "no_such_kind")
}

fn store_unavailable(error: anyhow::Error) -> Response {
	response::failure_with(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error)
}

async fn nodes(State(registry): State<Shared>) -> Response {
	match registry.store.resources(Kind::Node).await {
		Ok(listed) => response::success(StatusCode::OK, listed),
		Err(error) => store_unavailable(error),
	}
}

#[derive(serde::Deserialize)]
struct AppsAsked {
	node: Option<String>,
}

/// Every app, or only those placed on `?node=`; the list's revision is the newest change either
/// way, since the filter narrows what is answered rather than what it was read at.
async fn apps(State(registry): State<Shared>, Query(asked): Query<AppsAsked>) -> Response {
	match registry.store.resources(Kind::App).await {
		Ok(mut listed) => {
			if let Some(node) = &asked.node {
				listed.items.retain(|app| app::is_placed_on(&app.spec, node));
			}
			response::success(StatusCode::OK, listed)
		}
		Err(error) => store_unavailable(error),
	}
}

async fn node(Path(name): Path<String>, State(registry): State<Shared>) -> Response {
	match registry.store.resource(Kind::Node, &name).await {
		Ok(Some(node)) => response::success(StatusCode::OK, node),
		Ok(None) => response::failure(StatusCode::NOT_FOUND, "no_such_node"),
		Err(error) => store_unavailable(error),
	}
}

async fn app(Path(name): Path<String>, State(registry): State<Shared>) -> Response {
	match registry.store.resource(Kind::App, &name).await {
		Ok(Some(app)) => response::success(StatusCode::OK, app),
		Ok(None) => response::failure(StatusCode::NOT_FOUND, "no_such_resource"),
		Err(error) => store_unavailable(error),
	}
}

/// What a write's body says: who, the revision it was read at, and the spec wanted.
#[derive(Debug, PartialEq)]
struct Written {
	by: String,
	revision: i64,
	spec: Value,
}

/// The body of a `PUT`, or its refusal. `status` and `drift` are a controller's and the platform's
/// to write, never a person's, so a body carrying either is refused whatever it holds.
fn written(body: Result<axum::Json<Value>, JsonRejection>) -> Result<Written, Response> {
	let invalid =
		|message: &str| response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message);
	let Ok(axum::Json(Value::Object(mut body))) = body else {
		return Err(response::failure(StatusCode::BAD_REQUEST, "invalid_body"));
	};
	if let Some(key) = ["status", "drift"].into_iter().find(|key| body.contains_key(*key)) {
		return Err(response::failure_with(
			StatusCode::BAD_REQUEST,
			"forbidden_field",
			format!("`{key}` is not written by a person"),
		));
	}
	let Some(by) = body.get("by").and_then(Value::as_str).filter(|by| by_is_valid(by)) else {
		return Err(invalid("`by` names who"));
	};
	let Some(revision) = body.get("revision").and_then(Value::as_i64).filter(|r| *r >= 0) else {
		return Err(invalid("`revision` is the one read, 0 for what does not exist yet"));
	};
	let by = by.to_owned();
	let Some(spec) = body.remove("spec") else { return Err(invalid("`spec` is required")) };
	Ok(Written { by, revision, spec })
}

/// Sets a node, held to the revision read, with its spec checked as `PUT /nodes` checks a record.
async fn put_node(
	Path(name): Path<String>,
	State(registry): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	if let Some(refusal) = refused(&registry, &headers) {
		return refusal;
	}
	if !node::name_is_valid(&name) {
		return invalid_name();
	}
	let written = match written(body) {
		Ok(written) => written,
		Err(refusal) => return refusal,
	};
	match node::record(written.spec) {
		Ok(record) => set(&registry, Kind::Node, &name, written.by, written.revision, record).await,
		Err(message) => response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message),
	}
}

/// A write to an app's refusal, if it has one, or what it says: the same token as every write,
/// and a name that is one DNS label.
fn app_written(
	registry: &Shared,
	headers: &HeaderMap,
	name: &str,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Result<Written, Response> {
	if let Some(refusal) = refused(registry, headers) {
		return Err(refusal);
	}
	if !app::name_is_valid(name) {
		return Err(response::failure_with(
			StatusCode::BAD_REQUEST,
			"invalid_name",
			"An app's name is one DNS label, as its service.toml names it",
		));
	}
	written(body)
}

/// Sets an app's whole spec, held to the revision read.
async fn put_app(
	Path(name): Path<String>,
	State(registry): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let written = match app_written(&registry, &headers, &name, body) {
		Ok(written) => written,
		Err(refusal) => return refusal,
	};
	match app::spec(written.spec) {
		Ok(spec) => set(&registry, Kind::App, &name, written.by, written.revision, spec).await,
		Err(refusal) => refused_app(refusal),
	}
}

/// Sets the keys of an app's spec given and keeps every other, held to the revision read as `PUT`
/// is: a release is written often and by a machine, where it runs seldom and by a person, and
/// neither erases the other. The spec it makes is checked whole.
async fn patch_app(
	Path(name): Path<String>,
	State(registry): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let written = match app_written(&registry, &headers, &name, body) {
		Ok(written) => written,
		Err(refusal) => return refusal,
	};
	let Value::Object(given) = written.spec else {
		let message = "`spec` is an object of the fields changed";
		return response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message);
	};
	let kept = match registry.store.resource(Kind::App, &name).await {
		Ok(Some(kept)) => kept,
		Ok(None) => return response::failure(StatusCode::NOT_FOUND, "no_such_resource"),
		Err(error) => return store_unavailable(error),
	};
	// Read outside the write's lock, so `set` holds the write to the same revision once more.
	if kept.revision != written.revision {
		return response::failure(StatusCode::CONFLICT, "invalid_revision");
	}
	let Value::Object(mut spec) = kept.spec else {
		return store_unavailable(anyhow::anyhow!("{name}'s stored spec is not an object"));
	};
	spec.extend(given);
	match app::spec(Value::Object(spec)) {
		Ok(spec) => set(&registry, Kind::App, &name, written.by, written.revision, spec).await,
		Err(refusal) => refused_app(refusal),
	}
}

fn refused_app(refusal: Refusal) -> Response {
	match refusal {
		Refusal::Forbidden(message) => {
			response::failure_with(StatusCode::BAD_REQUEST, "forbidden_field", message)
		}
		Refusal::Invalid(message) => {
			response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message)
		}
	}
}

/// A write's answer: the revision it made, `{}` when there was nothing to change, and a conflict
/// when the resource changed after the revision given was read.
async fn set(
	registry: &Shared,
	kind: Kind,
	name: &str,
	by: String,
	revision: i64,
	spec: Map<String, Value>,
) -> Response {
	match registry.store.set(kind, name, &by, revision, Value::Object(spec)).await {
		Ok(Set::Changed(revision)) => {
			response::success(StatusCode::OK, json!({ "revision": revision }))
		}
		Ok(Set::Unchanged) => response::success(StatusCode::OK, json!({})),
		Ok(Set::Stale) => response::failure(StatusCode::CONFLICT, "invalid_revision"),
		Err(error) => store_unavailable(error),
	}
}

#[cfg(test)]
mod tests;
