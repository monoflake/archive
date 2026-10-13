//! The control contract: every kind's resources, and the catalog that describes them, under `/v1`,
//! each kind's routes the same handlers. Reading is open as the routes above it are, and writing
//! takes the same token. See RFC cpc2b.

use super::{Shared, by_is_valid, invalid_name, refused};
use crate::kinds::{self, CATALOG, Kind, Refusal};
use crate::secrets::{self, Secrets, Untaken};
use crate::store::{Key, Resource, Set};
use crate::{account, app, domain, node, task};
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{any, get};
use serde_json::{Map, Value, json};

/// Every kind the registry holds, each answered at `/v1/<plural>`.
const KINDS: [Kind; 4] = [Kind::Node, Kind::App, Kind::Account, Kind::Domain];

pub fn routes() -> Router<Shared> {
	let mut router = Router::new().route("/v1/kinds", get(kinds));
	for kind in KINDS {
		router = router.merge(resources(kind));
	}
	router.route("/v1/{kind}", any(no_such_kind)).route("/v1/{kind}/{name}", any(no_such_kind))
}

/// One kind's collection and its members, by id or by name.
fn resources(kind: Kind) -> Router<Shared> {
	let plural = kind.plural();
	let one = get(move |Path(key): Path<String>, State(registry): State<Shared>| {
		found_answer(registry, kind, key)
	})
	.put(move |Path(key): Path<String>, State(registry): State<Shared>, headers: HeaderMap, body| {
		put(registry, kind, key, headers, body)
	})
	.patch(
		move |Path(key): Path<String>, State(registry): State<Shared>, headers: HeaderMap, body| {
			patch(registry, kind, key, headers, body)
		},
	);
	let list = get(move |State(registry): State<Shared>, Query(asked): Query<Asked>| {
		listed(registry, kind, asked)
	});
	Router::new().route(&format!("/v1/{plural}"), list).route(&format!("/v1/{plural}/{{key}}"), one)
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
struct Asked {
	name: Option<String>,
	/// An app's alone: those placed on the node named.
	node: Option<String>,
}

/// Every resource of a kind, or the one `?name=` names, or the apps placed on `?node=`; the list's
/// revision is the newest change either way, since a filter narrows what is answered rather than
/// what it was read at.
async fn listed(registry: Shared, kind: Kind, asked: Asked) -> Response {
	match registry.store.resources(kind).await {
		Ok(mut listed) => {
			if let (Kind::App, Some(node)) = (kind, &asked.node) {
				listed.items.retain(|app| app::is_placed_on(&app.spec, node));
			}
			if let Some(name) = &asked.name {
				listed.items.retain(|resource| resource.name == *name);
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
		Kind::App | Kind::Account | Kind::Domain => {
			response::failure(StatusCode::NOT_FOUND, "no_such_resource")
		}
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

async fn found_answer(registry: Shared, kind: Kind, key: String) -> Response {
	match found(&registry, kind, &key).await {
		Ok(resource) => response::success(StatusCode::OK, resource),
		Err(refusal) => *refusal,
	}
}

/// What a write's body says: who, the revision it was read at, and the spec wanted. Not `Debug`: a
/// spec may carry an identity's value, which nothing here may print.
struct Written {
	by: String,
	revision: i64,
	spec: Value,
}

/// The body of a write, or its refusal. `status` and `drift` are a controller's and the platform's
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
			Kind::App | Kind::Account | Kind::Domain if !kinds::label_is_valid(name) => {
				return Err(Box::new(response::failure_with(
					StatusCode::BAD_REQUEST,
					"invalid_name",
					"A name is one DNS label, and never 16 hexadecimal digits, which is an id",
				)));
			}
			Kind::Node | Kind::App | Kind::Account | Kind::Domain => {}
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

/// Sets a resource's whole spec, held to the revision read.
async fn put(
	registry: Shared,
	kind: Kind,
	key: String,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let written = match asked(&registry, &headers, kind, &key, body) {
		Ok(written) => written,
		Err(refusal) => return *refusal,
	};
	let name = match named(&registry, kind, &key).await {
		Ok(name) => name,
		Err(refusal) => return *refusal,
	};
	match checked(&registry, kind, written.spec).await {
		Ok(spec) => set(&registry, kind, &name, written.by, written.revision, spec).await,
		Err(refusal) => *refusal,
	}
}

/// Sets the keys of a resource's spec given and keeps every other, held to the revision read as
/// `PUT` is: an app's release written by a machine and its placements by a person, or a node's
/// `retired` by an operator and the rest by the import, never erase each other.
async fn patch(
	registry: Shared,
	kind: Kind,
	key: String,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let written = match asked(&registry, &headers, kind, &key, body) {
		Ok(written) => written,
		Err(refusal) => return *refusal,
	};
	let (name, spec) = match merged(&registry, kind, &key, written.revision, written.spec).await {
		Ok(merged) => merged,
		Err(refusal) => return *refusal,
	};
	match checked(&registry, kind, spec).await {
		Ok(spec) => set(&registry, kind, &name, written.by, written.revision, spec).await,
		Err(refusal) => *refusal,
	}
}

/// What a `PATCH` makes, and the name of what it writes: `given` laid over the spec kept, which is
/// still at `revision`, a key given `null` taken out, as a JSON merge patch does. Read outside the
/// write's lock, so `set` holds the write to the same revision once more; the spec it makes is
/// checked whole by the kind's own rules.
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
	for (key, value) in given {
		if value.is_null() {
			spec.remove(&key);
		} else {
			spec.insert(key, value);
		}
	}
	Ok((kept.name, Value::Object(spec)))
}

/// `spec` as its kind stores it, or why it is not stored.
async fn checked(
	registry: &Shared,
	kind: Kind,
	spec: Value,
) -> Result<Map<String, Value>, Refused> {
	let spec = match kind {
		Kind::Node => node::record(spec)
			.map_err(|message| response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message)),
		Kind::App => app::spec(spec).map_err(refused_spec),
		Kind::Account => account::spec(spec).map_err(refused_spec),
		Kind::Domain => domain::spec(spec).map_err(refused_spec),
	};
	let spec = spec.map_err(Box::new)?;
	if kind == Kind::Node {
		referenced(registry, &spec).await?;
	}
	if kind == Kind::Account && registry.secrets.is_none() && account::holds_values(&spec) {
		return Err(Box::new(response::failure_with(
			StatusCode::SERVICE_UNAVAILABLE,
			"service_unavailable",
			"An identity's value is refused: this registry has no SECRETS or SECRETS_SCOPE_TOKEN set",
		)));
	}
	Ok(spec)
}

/// A node's account and domain, where each names an id, are resources the registry holds.
async fn referenced(registry: &Shared, record: &Map<String, Value>) -> Result<(), Refused> {
	for (key, kind) in [("account", Kind::Account), ("domain", Kind::Domain)] {
		let Some(id) = record.get(key).and_then(Value::as_str).filter(|id| kinds::is_id(id)) else {
			continue;
		};
		match registry.store.resource(kind, Key::Id(id)).await {
			Ok(Some(_)) => {}
			Ok(None) => {
				let message = format!("`{key}` names no {} the registry holds", kind.name());
				return Err(Box::new(response::failure_with(
					StatusCode::BAD_REQUEST,
					"invalid_resource",
					message,
				)));
			}
			Err(error) => return Err(Box::new(store_unavailable(error))),
		}
	}
	Ok(())
}

fn refused_spec(refusal: Refusal) -> Response {
	match refusal {
		Refusal::Forbidden(message) => {
			response::failure_with(StatusCode::BAD_REQUEST, "forbidden_field", message)
		}
		Refusal::Invalid(message) => {
			response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message)
		}
	}
}

/// Each identity's value handed to the secret service under `account`'s id, its name kept in the
/// value's place; the first the secret service does not take ends the write, which changes nothing
/// here.
async fn handed(
	secrets: &Secrets,
	account: &str,
	by: &str,
	mut spec: Map<String, Value>,
) -> Result<Value, Refused> {
	if let Some(Value::Array(identities)) = spec.get_mut("identities") {
		for identity in identities {
			let Some(value) = identity.get("value").and_then(Value::as_str) else { continue };
			let kind = identity["kind"].as_str().unwrap_or_default().to_owned();
			if let Err(untaken) = secrets.put(&secrets::name_of(account, &kind), value, by).await {
				return Err(Box::new(not_taken(&untaken)));
			}
			*identity = json!({ "kind": kind, "secret": secrets::kept_as(account, &kind) });
		}
	}
	Ok(Value::Object(spec))
}

/// The secret service unreachable or refusing, answered as what is behind the registry, never with
/// the value.
fn not_taken(untaken: &Untaken) -> Response {
	let message = format!("An identity's value was not kept, so nothing changed: {untaken}");
	response::failure_with(StatusCode::BAD_GATEWAY, "upstream_unavailable", message)
}

/// A write's answer: `202` with the revision it made and the task it is, told to the ledger and
/// never waited on; `200` with `{}` when there was nothing to change, which makes no task; and a
/// conflict when the resource changed after the revision given was read -- RFC cpc2b. An
/// account's identity values reach the secret service before anything here is written.
async fn set(
	registry: &Shared,
	kind: Kind,
	name: &str,
	by: String,
	revision: i64,
	spec: Map<String, Value>,
) -> Response {
	let set = match &registry.secrets {
		Some(secrets) if kind == Kind::Account && account::holds_values(&spec) => {
			let by = by.as_str();
			let make = move |account: String| async move { handed(secrets, &account, by, spec).await };
			match registry.store.set_with(kind, name, by, revision, make).await {
				Ok(Ok(set)) => Ok(set),
				Ok(Err(refusal)) => return *refusal,
				Err(error) => Err(error),
			}
		}
		_ => registry.store.set(kind, name, &by, revision, Value::Object(spec)).await,
	};
	match set {
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
