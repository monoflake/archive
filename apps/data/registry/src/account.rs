//! What an account's spec has to be before it is stored: a third party's account, which has no
//! name and is its id, and the identities the author finds it by, kept and answered, which the
//! catalog marks `masked`. See RFC 32hd9, RFC cpc2b and spec/architecture/registry.md.

use crate::kinds::Refusal;
use serde_json::{Map, Value};

/// Every key an account's spec may hold.
const FIELDS: &[&str] = &["provider", "identities"];

/// Whose account it is, each by a whole word, never a short code.
pub const PROVIDERS: &[&str] = &["oracle", "azure", "racknerd", "backblaze", "cloudflare"];

/// What an identity is of: the provider's user id, its tenancy, the email it signs in with.
pub const IDENTITIES: &[&str] = &["user_id", "tenancy", "email"];

/// Every key an identity holds, each required.
const IDENTITY: &[&str] = &["kind", "value"];

/// The longest identity kept: an email or a provider's id is far shorter.
const MAX_VALUE_BYTES: usize = 1024;

/// The spec as stored: `provider`, one of `PROVIDERS`, and `identities`, one to three, each
/// `{kind, value}` and each kind at most once, so an account always keeps an identifier.
pub fn spec(spec: Value) -> Result<Map<String, Value>, Refusal> {
	let Value::Object(spec) = spec else {
		return Err(Refusal::Invalid("an account's spec is a JSON object".into()));
	};
	if let Some(key) = spec.keys().find(|key| !FIELDS.contains(&key.as_str())) {
		return Err(Refusal::Forbidden(format!("`{key}` is not a field of an account")));
	}
	if !spec.get("provider").and_then(Value::as_str).is_some_and(|it| PROVIDERS.contains(&it)) {
		return Err(Refusal::Invalid(format!("`provider` is one of {}", PROVIDERS.join(", "))));
	}
	let Some(Value::Array(identities)) = spec.get("identities") else {
		let message = "`identities` is required, a list of one to three `{kind, value}`";
		return Err(Refusal::Invalid(message.into()));
	};
	if identities.is_empty() || identities.len() > IDENTITIES.len() {
		let message = "`identities` holds one to three, so an account always keeps an identifier";
		return Err(Refusal::Invalid(message.into()));
	}
	if let Some(refusal) = identities.iter().find_map(identity_refusal) {
		return Err(refusal);
	}
	for (index, identity) in identities.iter().enumerate() {
		if identities[..index].iter().any(|earlier| earlier["kind"] == identity["kind"]) {
			let kind = &identity["kind"];
			let message = format!("`identities` names the kind {kind} twice; each is held once");
			return Err(Refusal::Invalid(message));
		}
	}
	Ok(spec)
}

fn identity_refusal(identity: &Value) -> Option<Refusal> {
	let Value::Object(identity) = identity else {
		return Some(Refusal::Invalid("an identity is an object of `kind` and `value`".into()));
	};
	if let Some(key) = identity.keys().find(|key| !IDENTITY.contains(&key.as_str())) {
		let message = format!("an identity holds `kind` and `value` alone, never `{key}`");
		return Some(Refusal::Forbidden(message));
	}
	if !identity.get("kind").and_then(Value::as_str).is_some_and(|kind| IDENTITIES.contains(&kind)) {
		return Some(Refusal::Invalid(format!(
			"an identity's `kind` is one of {}",
			IDENTITIES.join(", ")
		)));
	}
	if !identity.get("value").and_then(Value::as_str).is_some_and(value_is_valid) {
		let message = "an identity's `value` is required, one line of text, at most 1 KiB";
		return Some(Refusal::Invalid(message.into()));
	}
	None
}

/// One line, not empty, no longer than an identity is.
fn value_is_valid(value: &str) -> bool {
	!value.is_empty() && value.len() <= MAX_VALUE_BYTES && !value.contains(['\n', '\r', '\0'])
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn identity(kind: &str) -> Value {
		json!({ "kind": kind, "value": format!("{kind}@example.com") })
	}

	fn oracle() -> Value {
		json!({ "provider": "oracle", "identities": [identity("user_id"), identity("email")] })
	}

	/// An account at Oracle holding `identity` alone.
	fn holding(identity: Value) -> Value {
		json!({ "provider": "oracle", "identities": [identity] })
	}

	#[test]
	fn an_account_is_kept_whole_with_one_to_three_identities() {
		assert_eq!(spec(oracle()).map(Value::Object), Ok(oracle()));
		let every: Vec<Value> = IDENTITIES.iter().map(|kind| identity(kind)).collect();
		for identities in [vec![identity("tenancy")], every] {
			let given = json!({ "provider": "racknerd", "identities": identities });
			assert!(spec(given.clone()).is_ok(), "{given}");
		}
	}

	/// An account always keeps an identifier, and each of its kinds once.
	#[test]
	fn an_account_without_an_identity_or_with_a_kind_twice_is_refused() {
		let twice = [identity("email"), identity("user_id"), identity("email")];
		let four = [identity("email"), identity("user_id"), identity("tenancy"), identity("email")];
		for (given, rule) in [
			(json!({ "provider": "azure" }), "required"),
			(json!({ "provider": "azure", "identities": [] }), "one to three"),
			(json!({ "provider": "azure", "identities": four }), "one to three"),
			(json!({ "provider": "azure", "identities": twice }), "twice"),
		] {
			let Err(Refusal::Invalid(message)) = spec(given.clone()) else {
				panic!("{given} is taken");
			};
			assert!(message.contains(rule), "{given}: {message}");
		}
	}

	#[test]
	fn a_key_the_kind_does_not_hold_is_forbidden() {
		let keyed = |key: &str| {
			let mut held = identity("email");
			held[key] = json!("accounts/registry/ACCOUNT_A1_EMAIL");
			holding(held)
		};
		let mut named = oracle();
		named["name"] = json!("oci-2");
		let mut reported = oracle();
		reported["status"] = json!({});
		for given in [named, reported, keyed("secret"), keyed("password")] {
			assert!(matches!(spec(given.clone()), Err(Refusal::Forbidden(_))), "{given}");
		}
	}

	#[test]
	fn refuses_what_is_not_an_account() {
		let given = |kind: Value, value: Value| holding(json!({ "kind": kind, "value": value }));
		let provider =
			|provider: Value| json!({ "provider": provider, "identities": [identity("email")] });
		let invalid = [
			json!(null),
			json!([]),
			json!({}),
			provider(json!("OCI")),
			provider(json!("oci-2")),
			provider(json!("")),
			provider(json!(1)),
			json!({ "provider": "oracle", "identities": "x" }),
			holding(json!("x")),
			holding(json!({ "kind": "email" })),
			holding(json!({ "value": "me@example.com" })),
			given(json!("password"), json!("me@example.com")),
			given(json!("email"), json!("")),
			given(json!("email"), json!("two\nlines")),
			given(json!("email"), json!("x".repeat(MAX_VALUE_BYTES + 1))),
			given(json!("email"), json!(null)),
			given(json!("email"), json!(1)),
		];
		for given in invalid {
			assert!(matches!(spec(given.clone()), Err(Refusal::Invalid(_))), "{given}");
		}
	}

	#[test]
	fn every_identity_kind_is_taken() {
		for kind in IDENTITIES {
			assert!(spec(holding(identity(kind))).is_ok(), "{kind}");
		}
	}
}
