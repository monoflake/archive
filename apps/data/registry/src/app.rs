//! What an app's name and spec have to be before either is stored. An app's spec holds only what
//! the registry keeps of it, so any other key is refused rather than kept. See RFC ja5hm and
//! RFC cpc2b.

use crate::node;
use serde_json::{Map, Value};

/// Why a spec is not stored, said for whoever sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
	/// A key the kind does not let be written.
	Forbidden(String),
	/// A key it does, holding what it cannot be.
	Invalid(String),
}

/// One DNS label, as every service's `name` is: lowercase letters, digits and inner hyphens.
pub fn name_is_valid(name: &str) -> bool {
	(1..=63).contains(&name.len())
		&& name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
		&& !name.starts_with('-')
		&& !name.ends_with('-')
}

/// The spec as stored: `placements`, the nodes it runs on, each named once and in the order given.
pub fn spec(spec: Value) -> Result<Map<String, Value>, Refusal> {
	let Value::Object(spec) = spec else {
		return Err(Refusal::Invalid("an app's spec is a JSON object".into()));
	};
	if let Some(key) = spec.keys().find(|key| *key != "placements") {
		return Err(Refusal::Forbidden(format!("`{key}` is not a field of an app")));
	}
	let Some(placements) = spec.get("placements") else {
		return Err(Refusal::Invalid("`placements` is required".into()));
	};
	let Value::Array(placements) = placements else {
		return Err(Refusal::Invalid("`placements` is a list of node names".into()));
	};
	for (index, placement) in placements.iter().enumerate() {
		let Some(name) = placement.as_str().filter(|name| node::name_is_valid(name)) else {
			return Err(Refusal::Invalid(format!("`placements` holds {placement}, not a node name")));
		};
		if placements[..index].iter().any(|earlier| earlier == placement) {
			return Err(Refusal::Invalid(format!("`placements` names {name} twice")));
		}
	}
	Ok(spec)
}

/// Whether a stored spec places the app on `node`, for `GET /v1/apps?node=`.
pub fn is_placed_on(spec: &Value, node: &str) -> bool {
	spec["placements"].as_array().is_some_and(|placements| placements.iter().any(|p| p == node))
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn a_name_is_one_dns_label() {
		for name in ["registry", "web", "a", "geo-2", "0x", &"a".repeat(63)] {
			assert!(name_is_valid(name), "{name}");
		}
		for name in ["", "-web", "web-", "Web", "we_b", "we.b", "wéb", &"a".repeat(64)] {
			assert!(!name_is_valid(name), "{name}");
		}
	}

	#[test]
	fn placements_are_kept_in_the_order_given() {
		let given = json!({ "placements": ["tyo", "sin", "rdu"] });
		assert_eq!(spec(given.clone()).map(Value::Object), Ok(given));
		assert!(spec(json!({ "placements": [] })).is_ok());
	}

	#[test]
	fn any_other_key_is_forbidden() {
		for key in ["status", "drift", "image", "release"] {
			let given = json!({ "placements": ["tyo"], key: 1 });
			assert!(matches!(spec(given), Err(Refusal::Forbidden(_))), "{key}");
		}
	}

	#[test]
	fn refuses_placements_that_are_not_node_names_once_each() {
		for given in [
			json!(null),
			json!([]),
			json!({}),
			json!({ "placements": "tyo" }),
			json!({ "placements": ["workers"] }),
			json!({ "placements": ["TYO"] }),
			json!({ "placements": [1] }),
			json!({ "placements": ["tyo", "sin", "tyo"] }),
		] {
			assert!(matches!(spec(given.clone()), Err(Refusal::Invalid(_))), "{given}");
		}
	}

	#[test]
	fn placed_on_a_node_its_spec_names() {
		let stored = json!({ "placements": ["tyo", "sin"] });
		assert!(is_placed_on(&stored, "sin"));
		assert!(!is_placed_on(&stored, "rdu"));
		assert!(!is_placed_on(&json!({}), "sin"));
	}
}
