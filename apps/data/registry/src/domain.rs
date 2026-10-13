//! What a domain's spec has to be before it is stored: what fails together, as the author groups
//! it, named by its resource's name and grouped through the nodes that name it. See RFC 32hd9.

use crate::kinds::Refusal;
use serde_json::{Map, Value};

/// The spec as stored: `note`, a line for whoever reads it, or nothing.
pub fn spec(spec: Value) -> Result<Map<String, Value>, Refusal> {
	let Value::Object(spec) = spec else {
		return Err(Refusal::Invalid("a domain's spec is a JSON object".into()));
	};
	if let Some(key) = spec.keys().find(|key| *key != "note") {
		return Err(Refusal::Forbidden(format!("`{key}` is not a field of a domain")));
	}
	// Absent, not empty: a note with nothing to say is left out.
	if spec.get("note").is_some_and(|note| !note.as_str().is_some_and(|note| !note.trim().is_empty()))
	{
		return Err(Refusal::Invalid("`note` is a line of text, left out when there is none".into()));
	}
	Ok(spec)
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	#[test]
	fn a_domain_is_kept_whole_its_note_optional() {
		for given in [json!({}), json!({ "note": "Oracle's Tokyo and Singapore, one card" })] {
			assert_eq!(spec(given.clone()).map(Value::Object), Ok(given));
		}
	}

	#[test]
	fn refuses_what_is_not_a_domain() {
		for given in
			[json!(null), json!([]), json!({ "note": "" }), json!({ "note": " " }), json!({ "note": 1 })]
		{
			assert!(matches!(spec(given.clone()), Err(Refusal::Invalid(_))), "{given}");
		}
		for given in [json!({ "name": "oci-2" }), json!({ "drift": {} })] {
			assert!(matches!(spec(given.clone()), Err(Refusal::Forbidden(_))), "{given}");
		}
	}
}
