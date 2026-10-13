//! The control contract's first slice: nodes and apps as resources of a kind, and the catalog that
//! describes them, under `/v1`. Reading is open as the routes above it are, and writing takes the
//! same token. See RFC cpc2b.

use super::{Shared, by_is_valid, invalid_name, refused};
use crate::app::{self, Refusal};
use crate::kinds::{CATALOG, Kind};
use crate::node;
use crate::store::{Key, Resource, Set};
use crate::task;
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
		.route("/v1/nodes/{key}", get(node).put(put_node).patch(patch_node))
		.route("/v1/apps", get(apps))
		.route("/v1/apps/{key}", get(app).put(put_app).patch(patch_app))
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

#[derive(serde::Deserialize)]
struct NodesAsked {
	name: Option<String>,
}

/// Every node, or the one `?name=` names; the list's revision is the newest change either way.
async fn nodes(State(registry): State<Shared>, Query(asked): Query<NodesAsked>) -> Response {
	match registry.store.resources(Kind::Node).await {
		Ok(mut listed) => {
			if let Some(name) = &asked.name {
				listed.items.retain(|node| node.name == *name);
			}
			response::success(StatusCode::OK, listed)
		}
		Err(error) => store_unavailable(error),
	}
}

#[derive(serde::Deserialize)]
struct AppsAsked {
	node: Option<String>,
	name: Option<String>,
}

/// Every app, or only those placed on `?node=`, or the one `?name=` names; the list's revision is
/// the newest change either way, since a filter narrows what is answered rather than what it was
/// read at.
async fn apps(State(registry): State<Shared>, Query(asked): Query<AppsAsked>) -> Response {
	match registry.store.resources(Kind::App).await {
		Ok(mut listed) => {
			if let Some(node) = &asked.node {
				listed.items.retain(|app| app::is_placed_on(&app.spec, node));
			}
			if let Some(name) = &asked.name {
				listed.items.retain(|app| app.name == *name);
			}
			response::success(StatusCode::OK, listed)
		}
		Err(error) => store_unavailable(error),
	}
}

/// A refusal, answered as it is. Boxed: a response is large beside what the common path carries.
type Refused = Box<Response>;

/// What a kind answers for a resource it does not hold.
fn absent(kind: Kind) -> Response {
	match kind {
		Kind::Node => response::failure(StatusCode::NOT_FOUND, "no_such_node"),
		Kind::App => response::failure(StatusCode::NOT_FOUND, "no_such_resource"),
	}
}

/// The resource `key` names, by its id or by its name -- RFC 2kuvi.
async fn found(registry: &Shared, kind: Kind, key: &str) -> Result<Resource, Refused> {
	match registry.store.resource(kind, Key::of(key)).await {
		Ok(Some(resource)) => Ok(resource),
		Ok(None) => Err(Box::new(absent(kind))),
		Err(error) => Err(Box::new(store_unavailable(error))),
	}
}

async fn node(Path(key): Path<String>, State(registry): State<Shared>) -> Response {
	match found(&registry, Kind::Node, &key).await {
		Ok(node) => response::success(StatusCode::OK, node),
		Err(refusal) => *refusal,
	}
}

async fn app(Path(key): Path<String>, State(registry): State<Shared>) -> Response {
	match found(&registry, Kind::App, &key).await {
		Ok(app) => response::success(StatusCode::OK, app),
		Err(refusal) => *refusal,
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
fn written(body: Result<axum::Json<Value>, JsonRejection>) -> Result<Written, Refused> {
	let invalid = |message: &str| {
		Box::new(response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message))
	};
	let Ok(axum::Json(Value::Object(mut body))) = body else {
		return Err(Box::new(response::failure(StatusCode::BAD_REQUEST, "invalid_body")));
	};
	if let Some(key) = ["status", "drift"].into_iter().find(|key| body.contains_key(*key)) {
		return Err(Box::new(response::failure_with(
			StatusCode::BAD_REQUEST,
			"forbidden_field",
			format!("`{key}` is not written by a person"),
		)));
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

/// A write's refusal, if it has one, or what it says: the token every write takes, and, where the
/// key is a name, one its kind can have. A key of an id's shape is an id, and is looked up later.
fn asked(
	registry: &Shared,
	headers: &HeaderMap,
	kind: Kind,
	key: &str,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Result<Written, Refused> {
	if let Some(refusal) = refused(registry, headers) {
		return Err(Box::new(refusal));
	}
	if let Key::Name(name) = Key::of(key) {
		match kind {
			Kind::Node if !node::name_is_valid(name) => return Err(Box::new(invalid_name())),
			Kind::App if !app::name_is_valid(name) => {
				return Err(Box::new(response::failure_with(
					StatusCode::BAD_REQUEST,
					"invalid_name",
					"An app's name is one DNS label, as its service.toml names it, and never 16 \
					 hexadecimal digits, which is an id",
				)));
			}
			Kind::Node | Kind::App => {}
		}
	}
	written(body)
}

/// The name a write sets: the key itself when it is a name, since a resource is made by its name
/// alone, or the name of the resource the id names, which has to be held -- RFC 2kuvi.
async fn named(registry: &Shared, kind: Kind, key: &str) -> Result<String, Refused> {
	match Key::of(key) {
		Key::Name(name) => Ok(name.to_owned()),
		Key::Id(_) => found(registry, kind, key).await.map(|resource| resource.name),
	}
}

/// Sets a node's whole spec, held to the revision read, checked as node.rs checks a record.
async fn put_node(
	Path(key): Path<String>,
	State(registry): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let written = match asked(&registry, &headers, Kind::Node, &key, body) {
		Ok(written) => written,
		Err(refusal) => return *refusal,
	};
	match named(&registry, Kind::Node, &key).await {
		Ok(name) => set_node(&registry, &name, written.by, written.revision, written.spec).await,
		Err(refusal) => *refusal,
	}
}

/// Sets the keys of a node's spec given and keeps every other, as `PATCH /v1/apps` does: the
/// import writes what `nodes.toml` says, and an operator's `retired` is kept -- RFC qcyb3.
async fn patch_node(
	Path(key): Path<String>,
	State(registry): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let written = match asked(&registry, &headers, Kind::Node, &key, body) {
		Ok(written) => written,
		Err(refusal) => return *refusal,
	};
	match merged(&registry, Kind::Node, &key, written.revision, written.spec).await {
		Ok((name, spec)) => set_node(&registry, &name, written.by, written.revision, spec).await,
		Err(refusal) => *refusal,
	}
}

async fn set_node(
	registry: &Shared,
	name: &str,
	by: String,
	revision: i64,
	spec: Value,
) -> Response {
	match node::record(spec) {
		Ok(record) => set(registry, Kind::Node, name, by, revision, record).await,
		Err(message) => response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message),
	}
}

/// What a `PATCH` makes, and the name of what it writes: `given` laid over the spec kept, which is
/// still at `revision`. Read outside the write's lock, so `set` holds the write to the same
/// revision once more; the spec it makes is checked whole by the kind's own rules.
async fn merged(
	registry: &Shared,
	kind: Kind,
	key: &str,
	revision: i64,
	given: Value,
) -> Result<(String, Value), Refused> {
	let Value::Object(given) = given else {
		let message = "`spec` is an object of the fields changed";
		let invalid = response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message);
		return Err(Box::new(invalid));
	};
	let kept = found(registry, kind, key).await?;
	if kept.revision != revision {
		return Err(Box::new(response::failure(StatusCode::CONFLICT, "invalid_revision")));
	}
	let Value::Object(mut spec) = kept.spec else {
		let name = &kept.name;
		let broken = anyhow::anyhow!("{name}'s stored spec is not an object");
		return Err(Box::new(store_unavailable(broken)));
	};
	spec.extend(given);
	Ok((kept.name, Value::Object(spec)))
}

/// Sets an app's whole spec, held to the revision read.
async fn put_app(
	Path(key): Path<String>,
	State(registry): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let written = match asked(&registry, &headers, Kind::App, &key, body) {
		Ok(written) => written,
		Err(refusal) => return *refusal,
	};
	let name = match named(&registry, Kind::App, &key).await {
		Ok(name) => name,
		Err(refusal) => return *refusal,
	};
	match app::spec(written.spec) {
		Ok(spec) => set(&registry, Kind::App, &name, written.by, written.revision, spec).await,
		Err(refusal) => refused_app(refusal),
	}
}

/// Sets the keys of an app's spec given and keeps every other, held to the revision read as `PUT`
/// is: a release is written often and by a machine, where it runs seldom and by a person, and
/// neither erases the other.
async fn patch_app(
	Path(key): Path<String>,
	State(registry): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let written = match asked(&registry, &headers, Kind::App, &key, body) {
		Ok(written) => written,
		Err(refusal) => return *refusal,
	};
	let (name, spec) = match merged(&registry, Kind::App, &key, written.revision, written.spec).await
	{
		Ok(merged) => merged,
		Err(refusal) => return *refusal,
	};
	match app::spec(spec) {
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

/// A write's answer: `202` with the revision it made and the task it is, told to the ledger and
/// never waited on; `200` with `{}` when there was nothing to change, which makes no task; and a
/// conflict when the resource changed after the revision given was read -- RFC cpc2b.
async fn set(
	registry: &Shared,
	kind: Kind,
	name: &str,
	by: String,
	revision: i64,
	spec: Map<String, Value>,
) -> Response {
	match registry.store.set(kind, name, &by, revision, Value::Object(spec)).await {
		Ok(Set::Changed(written)) => {
			if let Some(ledger) = &registry.ledger {
				task::tell(ledger, kind, name, &by, &written);
			}
			let answer = json!({ "revision": written.revision, "task": written.task });
			response::success(StatusCode::ACCEPTED, answer)
		}
		Ok(Set::Unchanged) => response::success(StatusCode::OK, json!({})),
		Ok(Set::Stale) => response::failure(StatusCode::CONFLICT, "invalid_revision"),
		Err(error) => store_unavailable(error),
	}
}

#[cfg(test)]
mod tests;
