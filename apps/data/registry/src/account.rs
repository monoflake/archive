//! What an account's spec has to be before it is stored: a paid account at a provider, read by
//! its resource's name. An identity names where its value is kept and never holds it, since every
//! reader of the tailnet reads the registry; one written with its value has the secret service take
//! the value in its place. See RFC 32hd9 and spec/architecture/registry.md.

use crate::kinds::Refusal;
use serde_json::{Map, Value};

/// Every key an account's spec may hold.
const FIELDS: &[&str] = &["provider", "identities"];

/// What an identity is of: the provider's user id, its tenancy, the email it signs in with.
pub const IDENTITIES: &[&str] = &["user_id", "tenancy", "email"];

/// Every key an identity holds: its kind, and where its value is kept or, on a write alone, the
/// value itself, which is never stored.
const IDENTITY: &[&str] = &["kind", "secret", "value"];

/// The longest value the secret service keeps.
const MAX_VALUE_BYTES: usize = 64 * 1024;

/// The spec as stored: `provider`, a whole lowercase word, and `identities`, one to three, each
/// `{kind, secret}` and each kind at most once, so an account always keeps an identifier.
pub fn spec(spec: Value) -> Result<Map<String, Value>, Refusal> {
	let Value::Object(spec) = spec else {
		return Err(Refusal::Invalid("an account's spec is a JSON object".into()));
	};
	if let Some(key) = spec.keys().find(|key| !FIELDS.contains(&key.as_str())) {
		return Err(Refusal::Forbidden(format!("`{key}` is not a field of an account")));
	}
	if !spec.get("provider").and_then(Value::as_str).is_some_and(word_is_valid) {
		return Err(Refusal::Invalid("`provider` is a whole lowercase word, as `oracle`".into()));
	}
	let Some(Value::Array(identities)) = spec.get("identities") else {
		let message = "`identities` is required, a list of one to three `{kind, secret}`";
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
		return Some(Refusal::Invalid("an identity is an object of `kind` and `secret`".into()));
	};
	// Its value is the secret service's: a key that could carry it is refused, not kept.
	if let Some(key) = identity.keys().find(|key| !IDENTITY.contains(&key.as_str())) {
		let message = format!("an identity holds `kind` and `secret` or `value` alone, never `{key}`");
		return Some(Refusal::Forbidden(message));
	}
	if !identity.get("kind").and_then(Value::as_str).is_some_and(|kind| IDENTITIES.contains(&kind)) {
		return Some(Refusal::Invalid(format!(
			"an identity's `kind` is one of {}",
			IDENTITIES.join(", ")
		)));
	}
	match (identity.get("secret"), identity.get("value")) {
		(Some(_), Some(_)) => {
			let message = "an identity holds its `secret` or, written anew, its `value`, never both";
			Some(Refusal::Invalid(message.into()))
		}
		(Some(secret), None) if !secret.as_str().is_some_and(secret_is_valid) => {
			let message = "an identity's `secret` names a secret, as `scope/app/NAME`";
			Some(Refusal::Invalid(message.into()))
		}
		// Said without the value: a refusal is answered, and the value is answered nowhere.
		(None, Some(value)) if !value.as_str().is_some_and(value_is_valid) => {
			let message = "an identity's `value` is one line of text, at most 64 KiB";
			Some(Refusal::Invalid(message.into()))
		}
		(None, None) => {
			let message = "an identity holds its `secret`, or its `value` to be kept there";
			Some(Refusal::Invalid(message.into()))
		}
		_ => None,
	}
}

/// What the secret service takes: one line, not empty, no longer than it keeps.
fn value_is_valid(value: &str) -> bool {
	!value.is_empty() && value.len() <= MAX_VALUE_BYTES && !value.contains(['\n', '\r', '\0'])
}

/// Whether a spec, as `spec` passed it, carries any value the secret service has to take.
pub fn holds_values(spec: &Map<String, Value>) -> bool {
	let identities = spec.get("identities").and_then(Value::as_array);
	identities
		.is_some_and(|identities| identities.iter().any(|identity| identity.get("value").is_some()))
}

/// `oracle`, `azure`, `racknerd`: a provider is named by a whole word, never a short code.
fn word_is_valid(word: &str) -> bool {
	(1..=32).contains(&word.len()) && word.bytes().all(|b| b.is_ascii_lowercase())
}

/// A secret as the secret service names it: its scope and its app, each one DNS label, and its
/// name, a variable's -- spec/architecture/secrets.md.
fn secret_is_valid(secret: &str) -> bool {
	let mut parts = secret.split('/');
	let (Some(scope), Some(app), Some(name), None) =
		(parts.next(), parts.next(), parts.next(), parts.next())
	else {
		return false;
	};
	let label = |part: &str| {
		(1..=63).contains(&part.len())
			&& part.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
			&& !part.starts_with('-')
			&& !part.ends_with('-')
	};
	let variable = (1..=128).contains(&name.len())
		&& name.bytes().all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b == b'_')
		&& !name.as_bytes()[0].is_ascii_digit();
	label(scope) && label(app) && variable
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn identity(kind: &str) -> Value {
		json!({ "kind": kind, "secret": format!("platform/registry/{}", kind.to_uppercase()) })
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
	fn a_key_the_kind_does_not_hold_is_forbidden_an_identity_s_value_above_all() {
		let valued = |key: &str| {
			let mut held = identity("email");
			held[key] = json!("me@example.com");
			holding(held)
		};
		let mut named = oracle();
		named["name"] = json!("oci-2");
		let mut reported = oracle();
		reported["status"] = json!({});
		for given in [named, reported, valued("email"), valued("password")] {
			assert!(matches!(spec(given.clone()), Err(Refusal::Forbidden(_))), "{given}");
		}
		// Its `secret` and a `value` at once: never both, said without the value.
		let Err(Refusal::Invalid(message)) = spec(valued("value")) else { panic!("both taken") };
		assert!(message.contains("never both") && !message.contains("me@example.com"), "{message}");
	}

	#[test]
	fn refuses_what_is_not_an_account() {
		let given = |kind: Value, secret: Value| holding(json!({ "kind": kind, "secret": secret }));
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
			holding(json!({ "secret": "a/b/C" })),
			given(json!("password"), json!("a/b/C")),
			given(json!("email"), json!("C")),
			given(json!("email"), json!("a/C")),
			given(json!("email"), json!("a/b/c")),
			given(json!("email"), json!("a/b/C/D")),
			given(json!("email"), json!("A/b/C")),
			given(json!("email"), json!("a/b/1C")),
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
