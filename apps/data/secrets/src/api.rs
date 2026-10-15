//! What an operator writes, each node reads, and the cores ask each other, over the tailnet. Every
//! route that touches a value answers `503 service_unavailable` while the lock is sealed. A
//! `config` is answered to whoever may write its scope, a `secret` to its node alone, and a secret
//! is compared without being read. See RFC 231yq, spec/architecture/secrets.md, and RFC cpc2b for
//! the shape of a write and a list.

use crate::envelope::{self, Identity, Unopened};
use crate::peers;
use crate::seal::{RootKey, Seal};
use crate::store::{Class, Compared, Kept, Named, Set, Store, Wanted};
use crate::tokens::{self, Grant, NodeTokens, ScopeTokens};
use axum::Router;
use axum::extract::rejection::JsonRejection;
use axum::extract::{Path, Query, State};
use axum::http::header::RETRY_AFTER;
use axum::http::{HeaderMap, HeaderValue, StatusCode};
use axum::response::Response;
use axum::routing::{get, post};
use serde::Serialize;
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};
use std::sync::Arc;
use subtle::ConstantTimeEq;

pub struct Secrets {
	pub store: Store,
	pub seal: Arc<Seal>,
	/// What a bearer writing every scope has to be; empty, only the scope tokens write.
	pub token: String,
	/// The tokens that each write one scope alone, as digests.
	pub scopes: ScopeTokens,
	/// What a peer core's bearer has to be to be given this core's share; empty, none is given.
	pub peer_token: String,
	/// Each node's own read token, as its digest.
	pub nodes: NodeTokens,
	/// The scopes a node is given values of; any other scope's are held, given to no node.
	pub node_scopes: Vec<String>,
	/// Where control is asked which apps a node runs.
	pub control: peers::Control,
	/// What control last answered for each node, asked again conditionally.
	pub placements: peers::Placements,
	pub client: reqwest::Client,
}

pub type Shared = Arc<Secrets>;

/// The scope a value is in when a request names none.
const SCOPE: &str = "platform";
/// The longest value kept, far past any credential, so a mistake does not fill a row.
const MAX_VALUE_BYTES: usize = 64 * 1024;

pub fn routes(secrets: Shared) -> Router {
	Router::new()
		.route("/health", get(health))
		.route("/v1/seal", get(seal_state).post(unseal))
		.route("/v1/seal/share", get(share))
		.route("/v1/secrets", get(list))
		.route("/v1/secrets/{app}/{name}", get(read).put(write).delete(remove))
		.route("/v1/secrets/{app}/{name}/compare", post(compare))
		.route("/v1/nodes/{node}/secrets", get(node_secrets))
		.fallback(|| async { response::failure(StatusCode::NOT_FOUND, "no_such_route") })
		.with_state(secrets)
}

/// `SECRETS_NODE_SCOPES` read: scopes separated by whitespace, each one DNS label; unset or empty,
/// `platform` alone. A scope not named is never given to a node, whatever app it names, so a scope
/// added later is held until someone lists it. The first that is not a scope is the error.
pub fn node_scopes(value: &str) -> Result<Vec<String>, String> {
	let scopes: Vec<String> = value.split_whitespace().map(str::to_owned).collect();
	if let Some(wrong) = scopes.iter().find(|scope| !label_is_valid(scope)) {
		return Err(wrong.clone());
	}
	Ok(if scopes.is_empty() { vec![SCOPE.to_owned()] } else { scopes })
}

/// 16 lowercase hexadecimal digits, as control holds a node's id. See
/// spec/architecture/control.md, "Who reads it, and with what".
pub fn node_id_is_valid(id: &str) -> bool {
	id.len() == 16 && id.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One DNS label, as every service's and every scope's name is.
pub(crate) fn label_is_valid(name: &str) -> bool {
	(1..=63).contains(&name.len())
		&& name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
		&& !name.starts_with('-')
		&& !name.ends_with('-')
}

/// A value's name is a variable's, since host writes it into the app's `config.env` or
/// `secret.env`: capitals, digits and underscores, not starting with a digit.
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

fn unopened(error: Unopened) -> Response {
	eprintln!("secrets: {error}");
	response::failure_with(StatusCode::INTERNAL_SERVER_ERROR, "service_unavailable", error)
}

fn invalid_token() -> Response {
	response::failure(StatusCode::UNAUTHORIZED, "invalid_token")
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
	(!admitted).then(invalid_token)
}

/// The operator's write token alone, for what no scope's token may do: opening the lock.
fn write_refused(secrets: &Secrets, headers: &HeaderMap) -> Option<Response> {
	refused(&secrets.token, headers, "Writes are closed: this service has no token set")
}

/// What the bearer may write: every scope with `SECRETS_TOKEN`, one app of one scope with a scope's
/// token.
fn writer(secrets: &Secrets, headers: &HeaderMap) -> Result<Grant, Refusal> {
	let bearer = tokens::bearer(headers);
	if !secrets.token.is_empty() && tokens::equal(bearer.as_bytes(), secrets.token.as_bytes()) {
		return Ok(Grant::Every);
	}
	if let Some(grant) = secrets.scopes.grant_of(bearer) {
		return Ok(grant);
	}
	if secrets.token.is_empty() && secrets.scopes.is_empty() {
		return Err(Refusal::Closed);
	}
	Err(Refusal::Token)
}

/// Names that change how a process starts -- what it loads, runs or trusts -- which a scope's token
/// never writes: its value would be code run in the app it reaches. `SECRETS_TOKEN` writes them.
const STARTUP_PREFIXES: &[&str] = &["LD_", "DYLD_", "PYTHON", "PERL5"];
const STARTUP_NAMES: &[&str] = &[
	"NODE_OPTIONS",
	"NODE_PATH",
	"NODE_EXTRA_CA_CERTS",
	"RUBYOPT",
	"RUBYLIB",
	"PERLLIB",
	"BASH_ENV",
	"ENV",
	"PATH",
	"SHELLOPTS",
	"BASHOPTS",
	"PS4",
	"JAVA_TOOL_OPTIONS",
	"_JAVA_OPTIONS",
	"JDK_JAVA_OPTIONS",
	"GCONV_PATH",
	"SSL_CERT_FILE",
	"SSL_CERT_DIR",
];

/// The apps the service needs running before it can answer a node -- its database, and the old
/// registry while it still runs -- and itself. None takes a value from the service, or a
/// platform rebuilt from nothing could not start them: no token writes one in a scope a node is
/// given. See spec/architecture/secrets.md, "Who reads what".
const STARTS_THE_SERVICE: &[&str] =
	&["database", "postgres", "primary", "quorum", "registry", "secrets"];

/// Whether `name` is one a scope's token never writes.
fn changes_how_a_process_starts(name: &str) -> bool {
	STARTUP_NAMES.contains(&name) || STARTUP_PREFIXES.iter().any(|prefix| name.starts_with(prefix))
}

/// The value a request names, by `?scope=` and its path, once the bearer may write that app of that
/// scope, and what the bearer may write.
fn addressed<'a>(
	secrets: &Secrets,
	headers: &HeaderMap,
	scoped: &'a Scoped,
	app: &'a str,
	name: &'a str,
) -> Result<(Grant, Identity<'a>), Refusal> {
	let grant = writer(secrets, headers)?;
	let scope = scoped.scope.as_deref().unwrap_or(SCOPE);
	if !label_is_valid(scope) || !label_is_valid(app) {
		return Err(Refusal::Name);
	}
	if !variable_is_valid(name) {
		return Err(Refusal::Variable);
	}
	if !grant.reaches(scope, app) {
		return Err(Refusal::Token);
	}
	Ok((grant, Identity { scope, app, name }))
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
/// none, against a stored value; a key nothing can check is refused rather than trusted.
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
					"This core holds no share and no value to check the key against",
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

/// A value as a list or a read answers it: what it is, its class, its revision and when it changed,
/// and its value only when it is a `config`.
#[derive(Serialize)]
struct Item {
	#[serde(flatten)]
	named: Named,
	#[serde(skip_serializing_if = "Option::is_none")]
	value: Option<String>,
}

impl Drop for Item {
	fn drop(&mut self) {
		if let Some(value) = &mut self.value {
			zeroize::Zeroize::zeroize(value);
		}
	}
}

/// The value `kept` holds as text, opened under `root`.
fn opened(root: &RootKey, kept: &Kept) -> Result<String, Unopened> {
	let value = envelope::open(root, &kept.identity(), &kept.sealed)?;
	String::from_utf8(value.to_vec()).map_err(|_| Unopened)
}

/// `kept` as it is answered to whoever may write its scope: a `secret`'s value is never opened.
fn item(root: &RootKey, kept: &Kept) -> Result<Item, Unopened> {
	let value = match kept.named.class {
		Class::Config => Some(opened(root, kept)?),
		Class::Secret => None,
	};
	Ok(Item { named: kept.named.clone(), value })
}

/// Every value the bearer may write, in its scope alone for a scope's token: what each is, its
/// class and when it changed, and a `config`'s value, never a `secret`'s.
async fn list(State(secrets): State<Shared>, headers: HeaderMap) -> Response {
	let grant = match writer(&secrets, &headers) {
		Ok(grant) => grant,
		Err(refusal) => return refusal.answer(),
	};
	if secrets.seal.is_sealed() {
		return sealed();
	}
	let (kept, revision) = match secrets.store.list(grant.held()).await {
		Ok(listed) => listed,
		Err(error) => return store_unavailable(error),
	};
	let items = secrets
		.seal
		.with_key(|root| kept.iter().map(|kept| item(root, kept)).collect::<Result<Vec<_>, _>>());
	match items {
		None => sealed(),
		Some(Ok(items)) => {
			response::success(StatusCode::OK, json!({ "items": items, "revision": revision }))
		}
		Some(Err(error)) => unopened(error),
	}
}

#[derive(serde::Deserialize)]
struct Scoped {
	scope: Option<String>,
}

/// One value, as a list answers it.
async fn read(
	Path((app, name)): Path<(String, String)>,
	Query(scoped): Query<Scoped>,
	State(secrets): State<Shared>,
	headers: HeaderMap,
) -> Response {
	let identity = match addressed(&secrets, &headers, &scoped, &app, &name) {
		Ok((_, identity)) => identity,
		Err(refusal) => return refusal.answer(),
	};
	if secrets.seal.is_sealed() {
		return sealed();
	}
	let kept = match secrets.store.get(&identity).await {
		Ok(Some(kept)) => kept,
		Ok(None) => return response::failure(StatusCode::NOT_FOUND, "no_such_resource"),
		Err(error) => return store_unavailable(error),
	};
	match secrets.seal.with_key(|root| item(root, &kept)) {
		None => sealed(),
		Some(Ok(item)) => response::success(StatusCode::OK, item),
		Some(Err(error)) => unopened(error),
	}
}

/// Why a request is refused before anything is read, said for whoever sent it.
#[derive(Debug, PartialEq)]
enum Refusal {
	/// No token is set that writes anything.
	Closed,
	/// The bearer writes nothing, or not this scope.
	Token,
	/// A scope or an app that is not one DNS label.
	Name,
	/// A name that is not a variable's, or a value not on one line.
	Variable,
	/// A value longer than is kept.
	Size,
	/// A name a scope's token never writes.
	Startup(String),
	/// An app the service needs to start, in a scope a node is given.
	Starts(String),
	/// A key the route does not read.
	Forbidden(String),
	/// A body that is not one the route reads, and what is wrong with it if that can be said.
	Invalid(Option<&'static str>),
}

impl Refusal {
	fn answer(self) -> Response {
		match self {
			Refusal::Closed => response::failure_with(
				StatusCode::SERVICE_UNAVAILABLE,
				"service_unavailable",
				"Writes are closed: this service has no token set",
			),
			Refusal::Token => invalid_token(),
			Refusal::Name => response::failure_with(
				StatusCode::BAD_REQUEST,
				"invalid_name",
				"An app's name and a scope's are each one DNS label",
			),
			Refusal::Variable => response::failure(StatusCode::BAD_REQUEST, "invalid_variable"),
			Refusal::Size => response::failure(StatusCode::BAD_REQUEST, "invalid_size"),
			Refusal::Startup(name) => response::failure_with(
				StatusCode::BAD_REQUEST,
				"forbidden_field",
				format!(
					"`{name}` changes how a process starts, which only the write token writes, never a scope's"
				),
			),
			Refusal::Starts(app) => response::failure_with(
				StatusCode::BAD_REQUEST,
				"forbidden_field",
				format!(
					"`{app}` starts before this service answers a node, so it is given nothing from here"
				),
			),
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

/// A body's fields and who sent it, every key but `allowed` refused.
fn fields(
	body: Result<axum::Json<Value>, JsonRejection>,
	allowed: &[&str],
) -> Result<(String, Map<String, Value>), Refusal> {
	let Ok(axum::Json(Value::Object(body))) = body else {
		return Err(Refusal::Invalid(None));
	};
	if let Some(key) = body.keys().find(|key| !allowed.contains(&key.as_str())) {
		return Err(Refusal::Forbidden(format!("`{key}` is not a field of this route")));
	}
	let Some(by) = body.get("by").and_then(Value::as_str).filter(|by| !by.trim().is_empty()) else {
		return Err(Refusal::Invalid(Some("`by` names who")));
	};
	Ok((by.to_owned(), body))
}

/// A value offered in a body, or its refusal: a string on one line, no longer than is kept.
fn offered(fields: &mut Map<String, Value>) -> Result<Option<zeroize::Zeroizing<String>>, Refusal> {
	match fields.remove("value") {
		None => Ok(None),
		Some(Value::String(value)) => {
			let value = zeroize::Zeroizing::new(value);
			if value.contains(['\n', '\r', '\0']) {
				return Err(Refusal::Variable);
			}
			if value.len() > MAX_VALUE_BYTES {
				return Err(Refusal::Size);
			}
			Ok(Some(value))
		}
		Some(_) => Err(Refusal::Invalid(Some("`value` is a string"))),
	}
}

/// What a write's body says: who, the revision it was read at, the value and the class.
struct Written {
	by: String,
	revision: i64,
	value: Option<zeroize::Zeroizing<String>>,
	class: Option<Class>,
}

/// The body of a `PUT`, or its refusal: a value, a class, or both.
fn written(body: Result<axum::Json<Value>, JsonRejection>) -> Result<Written, Refusal> {
	let (by, mut fields) = fields(body, &["by", "revision", "value", "class"])?;
	let Some(revision) = fields.get("revision").and_then(Value::as_i64).filter(|r| *r >= 0) else {
		return Err(Refusal::Invalid(Some(
			"`revision` is the one read, 0 for what does not exist yet",
		)));
	};
	let class = match fields.get("class") {
		None => None,
		Some(class) => match class.as_str() {
			Some("config") => Some(Class::Config),
			Some("secret") => Some(Class::Secret),
			_ => return Err(Refusal::Invalid(Some("`class` is config or secret"))),
		},
	};
	let value = offered(&mut fields)?;
	if value.is_none() && class.is_none() {
		return Err(Refusal::Invalid(Some("`value` is a string, or `class` alone changes it")));
	}
	Ok(Written { by, revision, value, class })
}

/// Sets a value and its class, `secret` unless the write says `config`, or a class alone, held to
/// the revision read. A value and class equal to those kept write nothing; a class alone never
/// makes a `secret` readable.
async fn write(
	Path((app, name)): Path<(String, String)>,
	Query(scoped): Query<Scoped>,
	State(secrets): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let identity = match addressed(&secrets, &headers, &scoped, &app, &name) {
		Ok((Grant::Held { .. }, _)) if changes_how_a_process_starts(&name) => {
			return Refusal::Startup(name.clone()).answer();
		}
		Ok((_, identity)) if starts_the_service(&secrets, &identity) => {
			return Refusal::Starts(app.clone()).answer();
		}
		Ok((_, identity)) => identity,
		Err(refusal) => return refusal.answer(),
	};
	let written = match written(body) {
		Ok(written) => written,
		Err(refusal) => return refusal.answer(),
	};
	let set = match &written.value {
		Some(value) => {
			let Some(sealed_value) =
				secrets.seal.with_key(|root| envelope::seal(root, &identity, value.as_bytes()))
			else {
				return sealed();
			};
			let class = written.class.unwrap_or(Class::Secret);
			let same = |kept: &envelope::Sealed| {
				secrets
					.seal
					.with_key(|root| {
						envelope::open(root, &identity, kept).is_ok_and(|kept| *kept == value.as_bytes())
					})
					.unwrap_or(false)
			};
			let wanted = Wanted::Value(&sealed_value, class);
			secrets.store.set(&identity, &written.by, written.revision, wanted, same).await
		}
		None => {
			if secrets.seal.is_sealed() {
				return sealed();
			}
			let class = written.class.expect("a write without a value names a class");
			let wanted = Wanted::Class(class);
			secrets.store.set(&identity, &written.by, written.revision, wanted, |_| false).await
		}
	};
	match set {
		Ok(Set::Changed(revision)) => {
			response::success(StatusCode::OK, json!({ "revision": revision }))
		}
		Ok(Set::Unchanged) => response::success(StatusCode::OK, json!({})),
		Ok(Set::Stale) => response::failure(StatusCode::CONFLICT, "invalid_revision"),
		Ok(Set::Missing) => response::failure(StatusCode::NOT_FOUND, "no_such_resource"),
		Ok(Set::Revealing) => response::failure_with(
			StatusCode::BAD_REQUEST,
			"forbidden_field",
			"A secret becomes config only written again with its value",
		),
		Err(error) => store_unavailable(error),
	}
}

/// Whether `identity` names an app the service needs to start, in a scope a node is given.
fn starts_the_service(secrets: &Secrets, identity: &Identity<'_>) -> bool {
	STARTS_THE_SERVICE.contains(&identity.app)
		&& secrets.node_scopes.iter().any(|scope| scope == identity.scope)
}

/// Deletes a value, held to the revision read, leaving a change that says it was removed: each
/// node it was given to is told so, and takes it out of the app's environment.
async fn remove(
	Path((app, name)): Path<(String, String)>,
	Query(scoped): Query<Scoped>,
	State(secrets): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let identity = match addressed(&secrets, &headers, &scoped, &app, &name) {
		Ok((_, identity)) => identity,
		Err(refusal) => return refusal.answer(),
	};
	let (by, fields) = match fields(body, &["by", "revision"]) {
		Ok(read) => read,
		Err(refusal) => return refusal.answer(),
	};
	let Some(revision) = fields.get("revision").and_then(Value::as_i64).filter(|r| *r > 0) else {
		return Refusal::Invalid(Some("`revision` is the one read")).answer();
	};
	if secrets.seal.is_sealed() {
		return sealed();
	}
	match secrets.store.remove(&identity, &by, revision).await {
		Ok(Set::Changed(revision)) => {
			response::success(StatusCode::OK, json!({ "revision": revision }))
		}
		Ok(Set::Stale) => response::failure(StatusCode::CONFLICT, "invalid_revision"),
		Ok(_) => response::failure(StatusCode::NOT_FOUND, "no_such_resource"),
		Err(error) => store_unavailable(error),
	}
}

/// Whether two values are one, in time that depends on neither: each is hashed to a fixed length
/// first, so not even where they differ, or that their lengths do, shows in how long it takes.
fn same_value(held: &[u8], offered: &[u8]) -> bool {
	let (held, offered) = (Sha256::digest(held), Sha256::digest(offered));
	held.as_slice().ct_eq(offered.as_slice()).into()
}

/// Whether a value offered is the `secret` held, answered `{"matches": bool}` and never the value;
/// it takes the right to write that secret, each secret answers a few a minute across every copy,
/// and every answer is a row of the history before it is given. A `config` is read, not compared.
async fn compare(
	Path((app, name)): Path<(String, String)>,
	Query(scoped): Query<Scoped>,
	State(secrets): State<Shared>,
	headers: HeaderMap,
	body: Result<axum::Json<Value>, JsonRejection>,
) -> Response {
	let identity = match addressed(&secrets, &headers, &scoped, &app, &name) {
		Ok((_, identity)) => identity,
		Err(refusal) => return refusal.answer(),
	};
	let (by, mut fields) = match fields(body, &["by", "value"]) {
		Ok(read) => read,
		Err(refusal) => return refusal.answer(),
	};
	let value = match offered(&mut fields) {
		Ok(Some(value)) => value,
		Ok(None) => return Refusal::Invalid(Some("`value` is a string")).answer(),
		Err(refusal) => return refusal.answer(),
	};
	if secrets.seal.is_sealed() {
		return sealed();
	}
	let kept = match secrets.store.get(&identity).await {
		Ok(Some(kept)) => kept,
		Ok(None) => return response::failure(StatusCode::NOT_FOUND, "no_such_resource"),
		Err(error) => return store_unavailable(error),
	};
	if kept.named.class == Class::Config {
		return response::failure_with(
			StatusCode::BAD_REQUEST,
			"invalid_body",
			"A config value is read, not compared",
		);
	}
	let matches = secrets.seal.with_key(|root| {
		envelope::open(root, &identity, &kept.sealed).map(|held| same_value(&held, value.as_bytes()))
	});
	let matches = match matches {
		None => return sealed(),
		Some(Ok(matches)) => matches,
		Some(Err(error)) => return unopened(error),
	};
	match secrets.store.compared(&identity, &by, matches).await {
		Ok(Compared::Kept(_)) => response::success(StatusCode::OK, json!({ "matches": matches })),
		Ok(Compared::Limited(wait)) => {
			let mut refusal = response::failure_with(
				StatusCode::TOO_MANY_REQUESTS,
				"rate_limited",
				"This secret answers a few comparisons a minute; try again later",
			);
			let seconds = (wait.as_secs() + u64::from(wait.subsec_nanos() > 0)).max(1);
			refusal.headers_mut().insert(RETRY_AFTER, HeaderValue::from(seconds));
			refusal
		}
		Err(error) => store_unavailable(error),
	}
}

/// One value as a node is answered it, with its class, so host knows which file it goes in.
#[derive(Serialize)]
struct Opened {
	scope: String,
	app: String,
	name: String,
	class: Class,
	revision: i64,
	value: String,
}

impl Drop for Opened {
	fn drop(&mut self) {
		zeroize::Zeroize::zeroize(&mut self.value);
	}
}

/// The values of every app control places on `node`, a node's id, in the scopes a node is given
/// alone, to that node's own token alone; those removed, by name and revision, so a node takes
/// away only what was deleted; and the apps it looked at, so a node can tell an app it was not
/// answered for from one with nothing kept.
async fn node_secrets(
	Path(node): Path<String>,
	State(secrets): State<Shared>,
	headers: HeaderMap,
) -> Response {
	if !node_id_is_valid(&node) {
		return response::failure_with(
			StatusCode::BAD_REQUEST,
			"invalid_name",
			"A node is its id, 16 lowercase hexadecimal digits",
		);
	}
	if !secrets.nodes.admits(&node, tokens::bearer(&headers)) {
		return invalid_token();
	}
	if secrets.seal.is_sealed() {
		return sealed();
	}
	let Some(apps) =
		peers::placed_apps(&secrets.client, &secrets.control, &secrets.placements, &node).await
	else {
		return response::failure_with(
			StatusCode::BAD_GATEWAY,
			"upstream_unavailable",
			"Control did not answer which apps this node runs",
		);
	};
	let (kept, removed, revision) = match secrets.store.of_apps(&apps, &secrets.node_scopes).await {
		Ok(found) => found,
		Err(error) => return store_unavailable(error),
	};
	let opened = secrets.seal.with_key(|root| {
		kept
			.iter()
			.map(|kept| {
				Ok(Opened {
					scope: kept.named.scope.clone(),
					app: kept.named.app.clone(),
					name: kept.named.name.clone(),
					class: kept.named.class,
					revision: kept.named.revision,
					value: opened(root, kept)?,
				})
			})
			.collect::<Result<Vec<_>, Unopened>>()
	});
	match opened {
		None => sealed(),
		Some(Ok(items)) => {
			let answer =
				json!({ "items": items, "removed": removed, "apps": apps, "revision": revision });
			response::success(StatusCode::OK, answer)
		}
		Some(Err(error)) => unopened(error),
	}
}

#[cfg(test)]
pub(crate) mod tests;
