//! What a node's name and record have to be before either is stored. The keys checked are the ones
//! every reader relies on; any other is kept as given. See spec/architecture/registry.md.

use crate::kinds;
use serde_json::{Map, Value};
use std::net::Ipv4Addr;

pub const TIERS: &[&str] = &["datacenter", "home", "transient"];
pub const SYSTEMS: &[&str] = &["debian", "alpine"];
/// A provider's machine, or one in the author's own hands -- RFC 32hd9.
pub const HOSTINGS: &[&str] = &["datacenter", "selfhost"];
/// What a node holds: functions and services, data, the platform's database and schedules.
pub const ROLES: &[&str] = &["compute", "storage", "core"];

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
	// Read from its account, never written -- RFC 32hd9.
	if record.contains_key("domain") {
		return Err("`domain` is not written: a node's failure domain is its account".into());
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
	// RFC 32hd9's fields, optional while `nodes.toml` declares the old axes they are derived from.
	if record
		.get("hosting")
		.is_some_and(|hosting| !hosting.as_str().is_some_and(|h| HOSTINGS.contains(&h)))
	{
		return Err(format!("`hosting` is one of {}", HOSTINGS.join(", ")));
	}
	if record.get("account").is_some_and(|account| !account.as_str().is_some_and(kinds::is_id)) {
		return Err("`account` is the id of the account the node is under".into());
	}
	if record.get("roles").is_some_and(|roles| !roles_are_valid(roles)) {
		return Err(format!("`roles` is a list of one or more of {}, each once", ROLES.join(", ")));
	}
	// Where the place its IATA code names is, which the console draws and orders nodes by.
	if record.get("coordinates").is_some_and(|coordinates| !coordinates_are_valid(coordinates)) {
		return Err("`coordinates` is [latitude, longitude] in degrees".into());
	}
	// What the console calls it: its city first, its country last, and between them what tells it
	// from another node in that city, if anything does.
	if record.get("place").is_some_and(|place| !place.as_str().is_some_and(place_is_valid)) {
		return Err("`place` is \"City, Country\", or \"City, Part, Country\"".into());
	}
	Ok(record)
}

/// Two parts or more, each one written and unpadded, joined by `, `, 100 characters at most.
fn place_is_valid(place: &str) -> bool {
	let parts: Vec<&str> = place.split(", ").collect();
	place.chars().count() <= 100
		&& parts.len() >= 2
		&& parts.iter().all(|part| !part.is_empty() && part.trim() == *part && !part.contains(','))
}

/// `[latitude, longitude]`, each a number in degrees within its range.
fn coordinates_are_valid(coordinates: &Value) -> bool {
	let Value::Array(pair) = coordinates else { return false };
	let [Some(latitude), Some(longitude)] =
		[pair.first(), pair.get(1)].map(|n| n.and_then(Value::as_f64))
	else {
		return false;
	};
	pair.len() == 2 && (-90.0..=90.0).contains(&latitude) && (-180.0..=180.0).contains(&longitude)
}

fn roles_are_valid(roles: &Value) -> bool {
	let Value::Array(roles) = roles else { return false };
	!roles.is_empty()
		&& roles.iter().enumerate().all(|(index, role)| {
			role.as_str().is_some_and(|role| ROLES.contains(&role)) && !roles[..index].contains(role)
		})
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
			"system": "debian",
			"core": true,
			"expiry": 2036,
			"retired": false,
			"hosting": "datacenter",
			"account": "0123456789abcdef",
			"roles": ["compute", "core"],
			"coordinates": [1.3644, 103.9915],
			"place": "Singapore, Singapore",
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
		for optional in ["retired", "hosting", "account", "roles", "coordinates", "place"] {
			node.as_object_mut().unwrap().remove(optional);
		}
		assert!(record(node).is_ok());
	}

	#[test]
	fn every_required_key_is_required() {
		for key in ["address", "tier", "system"] {
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
			("domain", json!("oci")),
			("domain", json!("0123456789abcdef")),
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
			("hosting", json!("home")),
			("hosting", json!("Datacenter")),
			("hosting", json!(null)),
			("account", json!("oci")),
			("account", json!("oci-2")),
			("account", json!("0123456789ABCDEF")),
			("account", json!("0123456789abcde")),
			("account", json!(2)),
			("roles", json!([])),
			("roles", json!("compute")),
			("roles", json!(["compute", "compute"])),
			("roles", json!(["compute", "edge"])),
			("roles", json!([1])),
			("roles", json!(null)),
			("coordinates", json!([1.3644])),
			("coordinates", json!([1.3644, 103.9915, 0])),
			("coordinates", json!([91, 0])),
			("coordinates", json!([0, -181])),
			("coordinates", json!(["1.36", "103.99"])),
			("coordinates", json!({ "latitude": 1.3644, "longitude": 103.9915 })),
			("coordinates", json!(null)),
			("place", json!("")),
			("place", json!("Singapore")),
			("place", json!("Tokyo,Japan")),
			("place", json!("Tokyo, , Japan")),
			("place", json!(" Tokyo, Japan")),
			("place", json!("Tokyo, Japan ")),
			("place", json!(format!("{}, Japan", "x".repeat(100)))),
			("place", json!(["Tokyo", "Japan"])),
			("place", json!(null)),
		];
		for (key, value) in wrong {
			assert!(record(with(key, value.clone())).is_err(), "{key}: {value}");
		}
	}

	#[test]
	fn takes_a_place_of_a_city_and_a_country_with_a_part_between_them_or_none() {
		for place in ["Tokyo, Japan", "Tokyo, Narita, Japan", "Gävle, Sweden"] {
			assert!(record(with("place", json!(place))).is_ok(), "{place}");
		}
	}

	#[test]
	fn takes_every_hosting_and_any_roles_once_each() {
		for hosting in HOSTINGS {
			assert!(record(with("hosting", json!(hosting))).is_ok(), "{hosting}");
		}
		for roles in
			[json!(["compute"]), json!(["storage", "core"]), json!(["core", "storage", "compute"])]
		{
			assert!(record(with("roles", roles.clone())).is_ok(), "{roles}");
		}
	}
}
