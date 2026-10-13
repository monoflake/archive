//! What an operator writes, each node reads, and the cores ask each other, over the tailnet. Every
//! route that touches a secret answers `503 service_unavailable` while the lock is sealed. See
//! RFC 231yq, and RFC cpc2b for the shape of a write and a list.

use crate::envelope::{self, Identity, Unopened};
use crate::peers;
use crate::seal::{RootKey, Seal};
use crate::store::{Set, Store};
use crate::tokens::{self, NodeTokens};
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::Response;
use axum::routing::{get, put};
use serde::Serialize;
use serde_json::{Value, json};
use std::sync::Arc;

pub struct Secrets {
	pub store: Store,
	pub seal: Arc<Seal>,
	/// What a write's bearer has to be; empty, every write is refused.
	pub token: String,
	/// What a peer core's bearer has to be to be given this core's share; empty, none is given.
	pub peer_token: String,
	/// Each node's own read token, as its digest.
	pub nodes: NodeTokens,
	/// Where the registry is asked which apps a node runs.
	pub registries: Vec<String>,
	pub client: reqwest::Client,
}

pub type Shared = Arc<Secrets>;

/// The scope a secret is in when a request names none.
const SCOPE: &str = "platform";
/// The longest value kept, far past any credential, so a mistake does not fill a row.
const MAX_VALUE_BYTES: usize = 64 * 1024;

pub fn routes(secrets: Shared) -> Router {
	Router::new()
		.route("/health", get(health))
		.route("/v1/seal", get(seal_state).post(unseal))
		.route("/v1/seal/share", get(share))
		.route("/v1/secrets", get(list))
		.route("/v1/secrets/{app}/{name}", put(write))
		.route("/v1/nodes/{node}/secrets", get(node_secrets))
		.fallback(|| async { response::failure(StatusCode::NOT_FOUND, "no_such_route") })
		.with_state(secrets)
}

/// Three lowercase letters, an IATA code, as the registry holds a node's name.
pub fn node_name_is_valid(name: &str) -> bool {
	name.len() == 3 && name.bytes().all(|b| b.is_ascii_lowercase())
}

/// One DNS label, as every service's and every scope's name is.
fn label_is_valid(name: &str) -> bool {
	(1..=63).contains(&name.len())
		&& name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
		&& !name.starts_with('-')
		&& !name.ends_with('-')
}

/// A secret's name is a variable's, since host writes it into the app's `secret.env`: capitals,
/// digits and underscores, not starting with a digit.
fn variable_is_valid(name: &str) -> bool {
	(1..=128).contains(&name.len())
		&& name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
		&& !name.as_bytes()[0].is_ascii_digit()
}

fn sealed() -> Response {
	response::failure_with(
		StatusCode::SERVICE_UNAVAILABLE,
		"service_unavailable",
		"Sealed: two cores' shares, or the operator's key, open it",
	)
}

fn store_unavailable(error: anyhow::Error) -> Response {
	response::failure_with(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable", error)
}

/// A request's refusal when `token` is unset or its bearer is not it.
fn refused(token: &str, headers: &HeaderMap, unset: &str) -> Option<Response> {
	if token.is_empty() {
		return Some(response::failure_with(
			StatusCode::SERVICE_UNAVAILABLE,
			"service_unavailable",
			unset,
		));
	}
	let admitted = tokens::equal(tokens::bearer(headers).as_bytes(), token.as_bytes());
	(!admitted).then(|| response::failure(StatusCode::UNAUTHORIZED, "invalid_token"))
}

fn write_refused(secrets: &Secrets, headers: &HeaderMap) -> Option<Response> {
	refused(&secrets.token, headers, "Writes are closed: this service has no token set")
}

/// Healthy while the database answers, sealed or not: a sealed copy is waiting for its peers, and
/// failing its deploy for that would roll back a version that is doing what it should.
async fn health(State(secrets): State<Shared>) -> Response {
	match secrets.store.answers().await {
		Ok(()) => response::success(StatusCode::OK, json!({ "sealed": secrets.seal.is_sealed() })),
		Err(error) => {
			response::failure_with(StatusCode::SERVICE_UNAVAILABLE, "store_unavailable", error)
		}
	}
}

/// Whether the lock is sealed and whether this core holds its share, open to the tailnet.
async fn seal_state(State(secrets): State<Shared>) -> Response {
	let share = secrets.seal.share().is_some();
	response::success(StatusCode::OK, json!({ "sealed": secrets.seal.is_sealed(), "share": share }))
}

/// This core's share, to a peer core showing the peer token, and only while this core holds it.
async fn share(State(secrets): State<Shared>, headers: HeaderMap) -> Response {
	if let Some(refusal) =
		refused(&secrets.peer_token, &headers, "Shares are closed: this service has no peer token set")
	{
		return refusal;
	}
	match secrets.seal.share() {
		Some(share) => response::success(StatusCode::OK, share.written()),
		None => response::failure_with(
			StatusCode::SERVICE_UNAVAILABLE,
			"service_unavailable",
			"This core holds no share",
		),
	}
}

#[derive(serde::Deserialize)]
struct Unseal {
	key: String,
}

/// Opens the lock with the whole root key, for when fewer than two cores are left: the operator
/// decrypts it from the recovery ciphertext. It is checked against this core's share, or, with
/// none, against a stored secret; a key nothing can check is refused rather than trusted.
async fn unseal(
	State(secrets): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Unseal>, JsonRejection>,
) -> Response {
	if let Some(refusal) = write_refused(&secrets, &headers) {
		return refusal;
	}
	let Ok(axum::Json(mut body)) = body else {
		return response::failure(StatusCode::BAD_REQUEST, "invalid_body");
	};
	let key = RootKey::from_hex(&body.key);
	zeroize::Zeroize::zeroize(&mut body.key);
	let Some(key) = key else {
		return response::failure_with(
			StatusCode::BAD_REQUEST,
			"invalid_body",
			"`key` is 64 hex digits",
		);
	};
	let wrong =
		|| response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", "This is not the root key");
	if let Some(open) = secrets.seal.with_key(|held| held.check() == key.check()) {
		return if open {
			response::success(StatusCode::OK, json!({ "sealed": false }))
		} else {
			wrong()
		};
	}
	let matches = match secrets.seal.share() {
		Some(share) => share.check() == key.check(),
		None => match secrets.store.any().await {
			Ok(Some(kept)) => envelope::open(&key, &kept.identity(), &kept.sealed).is_ok(),
			Ok(None) => {
				return response::failure_with(
					StatusCode::BAD_REQUEST,
					"invalid_body",
					"This core holds no share and no secret to check the key against",
				);
			}
			Err(error) => return store_unavailable(error),
		},
	};
	if !matches {
		return wrong();
	}
	secrets.seal.open(key);
	eprintln!("secrets: unsealed by the operator's key");
	response::success(StatusCode::OK, json!({ "sealed": false }))
}

/// Every secret's name and revision, never a value, to the operator's write token alone: which
/// secrets exist is the operator's to know.
async fn list(State(secrets): State<Shared>, headers: HeaderMap) -> Response {
	if let Some(refusal) = write_refused(&secrets, &headers) {
		return refusal;
	}
	if secrets.seal.is_sealed() {
		return sealed();
	}
	match secrets.store.list().await {
		Ok(listed) => response::success(StatusCode::OK, listed),
		Err(error) => store_unavailable(error),
	}
}

#[derive(serde::Deserialize)]
struct Scoped {
	scope: Option<String>,
}

/// What a write's body says: who, the revision it was read at, and the value.
#[derive(Debug, PartialEq)]
struct Written {
	by: String,
	revision: i64,
	value: String,
}

/// Why a write's body is refused, said for whoever sent it.
#[derive(Debug, PartialEq)]
enum Refusal {
	/// A key a secret does not have.
	Forbidden(String),
	/// A body that is not a secret's, and what is wrong with it if that can be said.
	Invalid(Option<&'static str>),
}

impl Refusal {
	fn answer(self) -> Response {
		match self {
			Refusal::Forbidden(message) => {
				response::failure_with(StatusCode::BAD_REQUEST, "forbidden_field", message)
			}
			Refusal::Invalid(None) => response::failure(StatusCode::BAD_REQUEST, "invalid_body"),
			Refusal::Invalid(Some(message)) => {
				response::failure_with(StatusCode::BAD_REQUEST, "invalid_body", message)
			}
		}
	}
}

/// The body of a `PUT`, or its refusal. Every key but the three is refused.
fn written(body: Result<axum::Json<Value>, JsonRejection>) -> Result<Written, Refusal> {
	let Ok(axum::Json(Value::Object(mut body))) = body else {
		return Err(Refusal::Invalid(None));
	};
	if let Some(key) = body.keys().find(|key| !["by", "revision", "value"].contains(&key.as_str())) {
		return Err(Refusal::Forbidden(format!("`{key}` is not a field of a secret")));
	}
	let Some(by) = body.get("by").and_then(Value::as_str).filter(|by| !by.trim().is_empty()) else {
		return Err(Refusal::Invalid(Some("`by` names who")));
	};
	let Some(revision) = body.get("revision").and_then(Value::as_i64).filter(|r| *r >= 0) else {
		return Err(Refusal::Invalid(Some(
			"`revision` is the one read, 0 for what does not exist yet",
		)));
	};
	let by = by.to_owned();
	let Some(Value::String(value)) = body.remove("value") else {
		return Err(Refusal::Invalid(Some("`value` is a string")));
	};
	Ok(Written { by, revision, value })
}

/// Sets a secret, held to the revision read. A value equal to the one kept writes nothing.
async fn write(
	Path((app, name)): Path<(String, String)>,
	Query(scoped): Query<Scoped>,
	State(secrets): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	if let Some(refusal) = write_refused(&secrets, &headers) {
		return refusal;
	}
	let scope = scoped.scope.as_deref().unwrap_or(SCOPE);
	if !label_is_valid(scope) || !label_is_valid(&app) {
		return response::failure_with(
			StatusCode::BAD_REQUEST,
			"invalid_name",
			"An app's name and a scope's are each one DNS label",
		);
	}
	if !variable_is_valid(&name) {
		return response::failure(StatusCode::BAD_REQUEST, "invalid_variable");
	}
	let mut written = match written(body) {
		Ok(written) => written,
		Err(refusal) => return refusal.answer(),
	};
	let value = zeroize::Zeroizing::new(std::mem::take(&mut written.value));
	if value.contains(['\n', '\r', '\0']) {
		return response::failure(StatusCode::BAD_REQUEST, "invalid_variable");
	}
	if value.len() > MAX_VALUE_BYTES {
		return response::failure(StatusCode::BAD_REQUEST, "invalid_size");
	}
	let identity = Identity { scope, app: &app, name: &name };
	let Some(sealed_value) =
		secrets.seal.with_key(|root| envelope::seal(root, &identity, value.as_bytes()))
	else {
		return sealed();
	};
	let same = |kept: &envelope::Sealed| {
		secrets
			.seal
			.with_key(|root| {
				envelope::open(root, &identity, kept).is_ok_and(|kept| *kept == value.as_bytes())
			})
			.unwrap_or(false)
	};
	match secrets.store.set(&identity, &written.by, written.revision, &sealed_value, same).await {
		Ok(Set::Changed(revision)) => {
			response::success(StatusCode::OK, json!({ "revision": revision }))
		}
		Ok(Set::Unchanged) => response::success(StatusCode::OK, json!({})),
		Ok(Set::Stale) => response::failure(StatusCode::CONFLICT, "invalid_revision"),
		Err(error) => store_unavailable(error),
	}
}

/// One secret as a node is answered it.
#[derive(Serialize)]
struct Opened {
	scope: String,
	app: String,
	name: String,
	revision: i64,
	value: String,
}

impl Drop for Opened {
	fn drop(&mut self) {
		zeroize::Zeroize::zeroize(&mut self.value);
	}
}

/// The values of every app the registry places on `node`, to that node's own token alone.
async fn node_secrets(
	Path(node): Path<String>,
	State(secrets): State<Shared>,
	headers: HeaderMap,
) -> Response {
	if !node_name_is_valid(&node) {
		return response::failure_with(
			StatusCode::BAD_REQUEST,
			"invalid_name",
			"A node's name is three lowercase letters, an IATA code",
		);
	}
	if !secrets.nodes.admits(&node, tokens::bearer(&headers)) {
		return response::failure(StatusCode::UNAUTHORIZED, "invalid_token");
	}
	if secrets.seal.is_sealed() {
		return sealed();
	}
	let Some(apps) = peers::placed_apps(&secrets.client, &secrets.registries, &node).await else {
		return response::failure_with(
			StatusCode::BAD_GATEWAY,
			"upstream_unavailable",
			"No registry answered which apps this node runs",
		);
	};
	let (kept, revision) = match secrets.store.of_apps(&apps).await {
		Ok(found) => found,
		Err(error) => return store_unavailable(error),
	};
	let opened = secrets.seal.with_key(|root| {
		kept
			.iter()
			.map(|kept| {
				let value = envelope::open(root, &kept.identity(), &kept.sealed)?;
				let value = String::from_utf8(value.to_vec()).map_err(|_| Unopened)?;
				Ok(Opened {
					scope: kept.named.scope.clone(),
					app: kept.named.app.clone(),
					name: kept.named.name.clone(),
					revision: kept.named.revision,
					value,
				})
			})
			.collect::<Result<Vec<_>, Unopened>>()
	});
	match opened {
		None => sealed(),
		Some(Ok(items)) => {
			response::success(StatusCode::OK, json!({ "items": items, "revision": revision }))
		}
		Some(Err(unopened)) => {
			eprintln!("secrets: {unopened}");
			response::failure_with(StatusCode::INTERNAL_SERVER_ERROR, "service_unavailable", unopened)
		}
	}
}

#[cfg(test)]
pub(crate) mod tests;
