//! The control contract: every kind's resources, and the catalog that describes them, under `/v1`,
//! each kind's routes the same handlers. Reading is open as the routes above it are, and writing
//! takes the same token. See RFC cpc2b.

use super::{Shared, by_is_valid, invalid_name, refused};
use crate::kinds::{self, Kind, Refusal};
use crate::store::{Key, Resource, Set, Target};
use crate::{account, app, node, task};
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{any, get, post};
use serde_json::{Map, Value, json};

/// Every kind the registry holds, each answered at `/v1/<plural>`.
const KINDS: [Kind; 3] = [Kind::Node, Kind::App, Kind::Account];

pub fn routes() -> Router<Shared> {
	let mut router = Router::new().route("/v1/kinds", get(kinds));
	for kind in KINDS {
		router = router.merge(resources(kind));
	}
	router.route("/v1/{kind}", any(no_such_kind)).route("/v1/{kind}/{name}", any(no_such_kind))
}

/// One kind's collection and its members, by id or by name; a kind with no name is made by a
/// `POST` to its collection, which draws its id -- RFC 32hd9.
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
	let mut list = get(move |State(registry): State<Shared>, Query(asked): Query<Asked>| {
		listed(registry, kind, asked)
	});
	list = if kind.named() {
		list.merge(post(move || made_by_name(kind)))
	} else {
		list.merge(post(move |State(registry): State<Shared>, headers: HeaderMap, body| {
			create(registry, kind, headers, body)
		}))
	};
	Router::new().route(&format!("/v1/{plural}"), list).route(&format!("/v1/{plural}/{{key}}"), one)
}

async fn kinds() -> Response {
	response::success(StatusCode::OK, kinds::ANSWER)
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
	if asked.name.is_some() && !kind.named() {
		return unnamed(kind);
	}
	match registry.store.resources(kind).await {
		Ok(mut listed) => {
			if let (Kind::App, Some(node)) = (kind, &asked.node) {
				listed.items.retain(|app| app::is_placed_on(&app.spec, node));
			}
			if let Some(name) = &asked.name {
				listed.items.retain(|resource| resource.name.as_deref() == Some(name.as_str()));
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
		Kind::App | Kind::Account => response::failure(StatusCode::NOT_FOUND, "no_such_resource"),
	}
}

/// A `POST` to a named kind's collection, which is made by its name instead.
async fn made_by_name(kind: Kind) -> Response {
	let message = format!("A {} is made by PUT /v1/{}/<name>", kind.name(), kind.plural());
	response::failure_with(StatusCode::METHOD_NOT_ALLOWED, "invalid_method", message)
}

/// What a name answers where a kind has none: refused, saying the id is what names one.
fn unnamed(kind: Kind) -> Response {
	let (name, plural) = (kind.name(), kind.plural());
	let message = format!("An {name} has no name: it is its id, made by POST /v1/{plural}");
	response::failure_with(StatusCode::BAD_REQUEST, "invalid_name", message)
}

/// The resource `key` names, by its id or by its name -- RFC 2kuvi; a kind with no name, by its id
/// alone.
async fn found(registry: &Shared, kind: Kind, key: &str) -> Result<Resource, Refused> {
	let key = Key::of(key);
	if matches!(key, Key::Name(_)) && !kind.named() {
		return Err(Box::new(absent(kind)));
	}
	match registry.store.resource(kind, key).await {
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
/// to write, never a person's, so a body carrying either is refused whatever it holds, and so is a
/// `name` for a kind that has none.
fn written(kind: Kind, body: Result<axum::Json<Value>, JsonRejection>) -> Result<Written, Refused> {
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
	if body.contains_key("name") && !kind.named() {
		let message = format!("An {} has no name to write", kind.name());
		return Err(Box::new(response::failure_with(
			StatusCode::BAD_REQUEST,
			"forbidden_field",
			message,
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
/// key is a name, one its kind can have. A key of an id's shape is an id, and is looked up later;
/// a kind with no name takes no other. `key` is none for a `POST`, which names nothing.
fn asked(
	registry: &Shared,
	headers: &HeaderMap,
	kind: Kind,
	key: Option<&str>,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Result<Written, Refused> {
	if let Some(refusal) = refused(registry, headers) {
		return Err(Box::new(refusal));
	}
	if let Some(Key::Name(name)) = key.map(Key::of) {
		match kind {
			_ if !kind.named() => return Err(Box::new(unnamed(kind))),
			Kind::Node if !node::name_is_valid(name) => return Err(Box::new(invalid_name())),
			Kind::App | Kind::Account if !kinds::label_is_valid(name) => {
				return Err(Box::new(response::failure_with(
					StatusCode::BAD_REQUEST,
					"invalid_name",
					"A name is one DNS label, and never 16 hexadecimal digits, which is an id",
				)));
			}
			Kind::Node | Kind::App | Kind::Account => {}
		}
	}
	written(kind, body)
}

/// What a write sets: the resource named by the key when it is a name, since a named resource is
/// made by its name alone, or the one the id names, which has to be held -- RFC 2kuvi.
async fn target(registry: &Shared, kind: Kind, key: &str) -> Result<String, Refused> {
	match Key::of(key) {
		Key::Name(name) => Ok(name.to_owned()),
		Key::Id(_) => found(registry, kind, key).await.map(|resource| resource.id),
	}
}

/// `target`'s answer as the store takes it.
fn targeted(key: &str) -> Target<'_> {
	match Key::of(key) {
		Key::Name(name) => Target::Named(name),
		Key::Id(id) => Target::Held(id),
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
	let written = match asked(&registry, &headers, kind, Some(&key), body) {
		Ok(written) => written,
		Err(refusal) => return *refusal,
	};
	let held = match target(&registry, kind, &key).await {
		Ok(held) => held,
		Err(refusal) => return *refusal,
	};
	match checked(&registry, kind, written.spec).await {
		Ok(spec) => set(&registry, kind, targeted(&held), written.by, written.revision, spec).await,
		Err(refusal) => *refusal,
	}
}

/// Makes a resource of a kind with no name, its id drawn here and answered beside the revision and
/// the task -- RFC 32hd9. It is made at revision 0, and a body at any other is refused.
async fn create(
	registry: Shared,
	kind: Kind,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let written = match asked(&registry, &headers, kind, None, body) {
		Ok(written) => written,
		Err(refusal) => return *refusal,
	};
	match checked(&registry, kind, written.spec).await {
		Ok(spec) => set(&registry, kind, Target::Unnamed, written.by, written.revision, spec).await,
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
	let written = match asked(&registry, &headers, kind, Some(&key), body) {
		Ok(written) => written,
		Err(refusal) => return *refusal,
	};
	let (id, spec) = match merged(&registry, kind, &key, written.revision, written.spec).await {
		Ok(merged) => merged,
		Err(refusal) => return *refusal,
	};
	match checked(&registry, kind, spec).await {
		Ok(spec) => set(&registry, kind, Target::Held(&id), written.by, written.revision, spec).await,
		Err(refusal) => *refusal,
	}
}

/// What a `PATCH` makes, and the id of what it writes: `given` laid over the spec kept, which is
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
		let id = &kept.id;
		let broken = anyhow::anyhow!("{id}'s stored spec is not an object");
		return Err(Box::new(store_unavailable(broken)));
	};
	for (key, value) in given {
		if value.is_null() {
			spec.remove(&key);
		} else {
			spec.insert(key, value);
		}
	}
	Ok((kept.id, Value::Object(spec)))
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
	};
	let spec = spec.map_err(Box::new)?;
	if kind == Kind::Node {
		referenced(registry, &spec).await?;
	}
	Ok(spec)
}

/// A node's account is one the registry holds.
async fn referenced(registry: &Shared, record: &Map<String, Value>) -> Result<(), Refused> {
	for (key, kind) in [("account", Kind::Account)] {
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

/// A write's answer: `202` with the revision it made and the task it is, told to the ledger and
/// never waited on, and the id of what a `POST` made; `200` with `{}` when there was nothing to
/// change, which makes no task; and a conflict when the resource changed after the revision given
/// was read -- RFC cpc2b.
async fn set(
	registry: &Shared,
	kind: Kind,
	target: Target<'_>,
	by: String,
	revision: i64,
	spec: Map<String, Value>,
) -> Response {
	match registry.store.set(kind, target, &by, revision, Value::Object(spec)).await {
		Ok(Set::Changed(written)) => {
			if let Some(ledger) = &registry.ledger {
				task::tell(ledger, kind, &by, &written);
			}
			let mut answer = json!({ "revision": written.revision, "task": written.task });
			if target == Target::Unnamed {
				answer["id"] = json!(written.id);
			}
			response::success(StatusCode::ACCEPTED, answer)
		}
		Ok(Set::Unchanged) => response::success(StatusCode::OK, json!({})),
		Ok(Set::Stale) => response::failure(StatusCode::CONFLICT, "invalid_revision"),
		Err(error) => store_unavailable(error),
	}
}

#[cfg(test)]
mod tests;
