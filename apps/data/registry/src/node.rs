//! What a node's name and record have to be before either is stored. The keys checked are the ones
//! every reader relies on; any other is kept as given. See spec/architecture/registry.md.

use serde_json::{Map, Value};
use std::net::Ipv4Addr;

pub const TIERS: &[&str] = &["datacenter", "home", "transient"];
pub const SYSTEMS: &[&str] = &["debian", "alpine"];

/// Three lowercase ASCII letters, an IATA code.
pub fn name_is_valid(name: &str) -> bool {
	name.len() == 3 && name.bytes().all(|b| b.is_ascii_lowercase())
}

/// The record as stored, or what is wrong with it, said for the operator who sent it.
pub fn record(node: Value) -> Result<Map<String, Value>, String> {
	let Value::Object(record) = node else { return Err("a node is a JSON object".into()) };
	let text = |key: &str| match record.get(key) {
		Some(Value::String(text)) => Ok(text.as_str()),
		Some(_) => Err(format!("`{key}` is a string")),
		None => Err(format!("`{key}` is required")),
	};
	if text("address")?.parse::<Ipv4Addr>().is_err() {
		return Err("`address` is the node's tailnet IPv4, a dotted quad".into());
	}
	if !TIERS.contains(&text("tier")?) {
		return Err(format!("`tier` is one of {}", TIERS.join(", ")));
	}
	if !domain_is_valid(text("domain")?) {
		return Err("`domain` is a provider code, as `oci` or `oci-2`".into());
	}
	if !SYSTEMS.contains(&text("system")?) {
		return Err(format!("`system` is one of {}", SYSTEMS.join(", ")));
	}
	if record.get("core").is_some_and(|core| !core.is_boolean()) {
		return Err("`core` is true or false".into());
	}
	// A node leaves by this mark, never by its record going missing -- RFC qcyb3.
	if record.get("retired").is_some_and(|retired| !retired.is_boolean()) {
		return Err("`retired` is true or false".into());
	}
	if record.get("expiry").is_some_and(|expiry| !expiry.as_u64().is_some_and(is_year)) {
		return Err("`expiry` is a year, as a whole number".into());
	}
	Ok(record)
}

/// `oci`, `az`, `oci-2`: the provider, and which of its accounts when there is more than one.
fn domain_is_valid(domain: &str) -> bool {
	let (letters, number) = domain.split_once('-').unwrap_or((domain, ""));
	!letters.is_empty()
		&& letters.bytes().all(|b| b.is_ascii_lowercase())
		&& (domain.len() == letters.len()
			|| (!number.is_empty() && number.bytes().all(|b| b.is_ascii_digit())))
}

fn is_year(year: u64) -> bool {
	(1000..=9999).contains(&year)
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn sin() -> Value {
		json!({
			"address": "100.64.0.9",
			"tier": "datacenter",
			"domain": "oci-2",
			"system": "debian",
			"core": true,
			"expiry": 2036,
			"retired": false,
			"canary": true,
			"grants": ["database"],
		})
	}

	fn without(key: &str) -> Value {
		let mut node = sin();
		node.as_object_mut().unwrap().remove(key);
		node
	}

	fn with(key: &str, value: Value) -> Value {
		let mut node = sin();
		node[key] = value;
		node
	}

	#[test]
	fn a_name_is_three_lowercase_letters() {
		for name in ["sin", "tyo", "rdu"] {
			assert!(name_is_valid(name), "{name}");
		}
		for name in ["", "si", "sinn", "SIN", "s1n", "si-", "sín"] {
			assert!(!name_is_valid(name), "{name}");
		}
	}

	#[test]
	fn a_whole_record_is_kept_with_every_other_key() {
		let record = record(sin()).unwrap();
		assert_eq!(Value::Object(record), sin());
	}

	#[test]
	fn the_optional_keys_may_be_left_out() {
		let mut node = without("core");
		node.as_object_mut().unwrap().remove("expiry");
		node.as_object_mut().unwrap().remove("retired");
		assert!(record(node).is_ok());
	}

	#[test]
	fn every_required_key_is_required() {
		for key in ["address", "tier", "domain", "system"] {
			assert_eq!(record(without(key)), Err(format!("`{key}` is required")));
		}
	}

	#[test]
	fn refuses_what_is_not_an_object() {
		for node in [json!(null), json!([]), json!("sin"), json!(1)] {
			assert!(record(node).is_err());
		}
	}

	#[test]
	fn refuses_each_key_out_of_its_range() {
		let wrong = [
			("address", json!("100.64.0")),
			("address", json!("100.64.0.256")),
			("address", json!("fd7a::1")),
			("address", json!(1)),
			("tier", json!("cloud")),
			("domain", json!("OCI")),
			("domain", json!("oci-")),
			("domain", json!("-2")),
			("domain", json!("oci-a")),
			("domain", json!("oci-2-3")),
			("domain", json!("")),
			("system", json!("ubuntu")),
			("core", json!("yes")),
			("core", json!(null)),
			("retired", json!("yes")),
			("retired", json!(1)),
			("retired", json!(null)),
			("expiry", json!("2036")),
			("expiry", json!(2036.5)),
			("expiry", json!(-1)),
			("expiry", json!(36)),
		];
		for (key, value) in wrong {
			assert!(record(with(key, value.clone())).is_err(), "{key}: {value}");
		}
	}

	#[test]
	fn takes_each_domain_nodes_toml_has() {
		for domain in ["oci", "oci-2", "az", "rkn", "int"] {
			assert!(domain_is_valid(domain), "{domain}");
		}
	}
}
