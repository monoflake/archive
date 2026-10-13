//! The secret service, as the registry hands it the value of a field the catalog marks `secret`:
//! written once to the first core that answers, with a token that reaches `accounts/registry`
//! alone, and never kept, logged or answered here. No field is marked so today; an account's
//! identities are `masked` instead. See spec/architecture/registry.md and
//! spec/architecture/secrets.md.

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::Request;
use hyper::header::{AUTHORIZATION, CONTENT_TYPE};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde_json::{Value, json};
use std::time::Duration;

/// The secret service's port on each core's tailnet address.
pub const PORT: u16 = 12014;

/// Where an identity's value is kept: a scope no node is given, and the registry's app in it.
pub const SCOPE: &str = "accounts";
pub const APP: &str = "registry";

/// One core's time to answer before the next is asked. A write waits on it, under the lock every
/// registry write holds, so it is short.
const PATIENCE: Duration = Duration::from_secs(5);

/// Why a value was not taken, said without it: no core answered, or one refused, by its code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Untaken {
	Unanswered,
	Refused(String),
}

impl std::fmt::Display for Untaken {
	fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		match self {
			Self::Unanswered => write!(f, "no core of the secret service answered"),
			Self::Refused(code) => write!(f, "the secret service refused it with `{code}`"),
		}
	}
}

pub struct Secrets {
	/// Each core's base, `http://<address>:<port>`, asked in order.
	cores: Vec<String>,
	token: String,
	client: Client<HttpConnector, Full<Bytes>>,
}

impl Secrets {
	pub fn new(cores: Vec<String>, token: String) -> Self {
		let client = Client::builder(TokioExecutor::new()).build(HttpConnector::new());
		Self { cores, token, client }
	}

	/// `SECRETS`, the cores' tailnet IPv4s separated by whitespace as the secret service's own
	/// `CORES` is, and `SECRETS_SCOPE_TOKEN`; none where either is unset, and a `value` is refused.
	pub fn from_env() -> Option<Self> {
		let cores: Vec<String> = std::env::var("SECRETS")
			.unwrap_or_default()
			.split_whitespace()
			.map(|address| format!("http://{address}:{PORT}"))
			.collect();
		let token = std::env::var("SECRETS_SCOPE_TOKEN").unwrap_or_default();
		(!cores.is_empty() && !token.trim().is_empty()).then(|| Self::new(cores, token))
	}

	/// `value` written as `accounts/registry/<name>`, a `secret`, by `by`: at the revision the core
	/// reads it at, 0 where it holds none. The first core that answers is the one written to.
	pub async fn put(&self, name: &str, value: &str, by: &str) -> Result<(), Untaken> {
		let path = format!("/v1/secrets/{APP}/{name}?scope={SCOPE}");
		for core in &self.cores {
			let Some((status, read)) = self.ask(core, "GET", &path, None).await else {
				continue;
			};
			let revision = match (status, code_of(&read).as_deref()) {
				(200, _) => read["data"]["revision"].as_i64().ok_or_else(|| refused(&read))?,
				(404, Some("no_such_resource")) => 0,
				_ => return Err(refused(&read)),
			};
			let body = json!({ "by": by, "revision": revision, "value": value, "class": "secret" });
			let Some((status, written)) = self.ask(core, "PUT", &path, Some(body)).await else {
				return Err(Untaken::Unanswered);
			};
			return if status == 200 { Ok(()) } else { Err(refused(&written)) };
		}
		Err(Untaken::Unanswered)
	}

	/// A core's status and envelope, or none where it did not answer one within `PATIENCE`.
	async fn ask(
		&self,
		core: &str,
		method: &str,
		path: &str,
		body: Option<Value>,
	) -> Option<(u16, Value)> {
		let bytes = body.map(|body| Bytes::from(body.to_string())).unwrap_or_default();
		let request = Request::builder()
			.method(method)
			.uri(format!("{core}{path}"))
			.header(AUTHORIZATION, format!("Bearer {}", self.token))
			.header(CONTENT_TYPE, "application/json")
			.body(Full::new(bytes))
			.ok()?;
		let asked = async {
			let answer = self.client.request(request).await.ok()?;
			let status = answer.status().as_u16();
			let bytes = answer.into_body().collect().await.ok()?.to_bytes();
			Some((status, serde_json::from_slice(&bytes).unwrap_or(Value::Null)))
		};
		tokio::time::timeout(PATIENCE, asked).await.ok().flatten()
	}
}

fn code_of(envelope: &Value) -> Option<String> {
	envelope["code"].as_str().map(str::to_owned)
}

/// A refusal by its code alone: what the secret service said beside it is not passed on.
fn refused(envelope: &Value) -> Untaken {
	Untaken::Refused(code_of(envelope).unwrap_or_else(|| "an answer it cannot read".into()))
}

/// The name an identity's value is kept under: the account's id and the identity's kind, in
/// capitals, so each account holds one of each kind -- `ACCOUNT_A3F8C91D7E42B650_EMAIL`.
pub fn name_of(account: &str, kind: &str) -> String {
	format!("ACCOUNT_{}_{}", account.to_uppercase(), kind.to_uppercase())
}

/// What the account keeps in a value's place: the secret's full name, `scope/app/NAME`.
pub fn kept_as(account: &str, kind: &str) -> String {
	format!("{SCOPE}/{APP}/{}", name_of(account, kind))
}

#[cfg(test)]
pub(crate) mod fake;

#[cfg(test)]
mod tests;
