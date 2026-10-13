use super::*;
use crate::app;
use serde_json::{Value, json};

/// Whether `kind`'s validation stores `spec`.
fn accepts(kind: &Description, spec: Value) -> bool {
	match kind.kind {
		Kind::Node => node::record(spec).is_ok(),
		Kind::App => app::spec(spec).is_ok(),
	}
}

/// A whole spec each kind stores, every field of the catalog in it; a node's keeps two keys more,
/// which it keeps as given.
fn example(kind: &Description) -> Value {
	match kind.kind {
		Kind::Node => json!({
			"address": "100.64.0.9",
			"tier": "datacenter",
			"domain": "oci-2",
			"system": "debian",
			"core": true,
			"expiry": 2036,
			"retired": false,
			"hosting": "datacenter",
			"account": "oci-2",
			"roles": ["compute", "core"],
			"canary": true,
			"grants": ["database"],
		}),
		Kind::App => json!({
			"placements": ["tyo", "sin"],
			"release": {
				"repository": "monoflake/platform",
				"run": 7,
				"commit": "0123456789abcdef0123456789abcdef01234567",
			},
		}),
	}
}

fn with(spec: &Value, key: &str, value: Value) -> Value {
	let mut spec = spec.clone();
	spec[key] = value;
	spec
}

fn wrong_types(type_: Type) -> Vec<Value> {
	match type_ {
		Type::String => vec![json!(1), json!(true), json!(null), json!(["x"])],
		Type::Integer => vec![json!("2036"), json!(2036.5), json!(true), json!(null)],
		Type::Boolean => vec![json!("yes"), json!(1), json!(null)],
		Type::Choice => vec![json!("none of them"), json!(1), json!(null)],
		Type::List => vec![json!("tyo"), json!(1), json!({}), json!(null)],
		Type::Object => vec![json!("x"), json!(1), json!([]), json!(null)],
	}
}

#[test]
fn every_example_is_stored_and_names_every_field() {
	for kind in CATALOG {
		let example = example(kind);
		assert!(accepts(kind, example.clone()), "{}", kind.kind.name());
		for field in kind.fields {
			assert!(example.get(field.name).is_some(), "{}: {}", kind.kind.name(), field.name);
		}
	}
}

#[test]
fn a_field_is_required_exactly_where_the_catalog_says() {
	for kind in CATALOG {
		for field in kind.fields {
			let mut spec = example(kind);
			spec.as_object_mut().unwrap().remove(field.name);
			assert_eq!(accepts(kind, spec), !field.required, "{}: {}", kind.kind.name(), field.name);
		}
	}
}

#[test]
fn a_value_not_of_its_type_is_refused() {
	for kind in CATALOG {
		let example = example(kind);
		for field in kind.fields {
			for value in wrong_types(field.type_) {
				let spec = with(&example, field.name, value.clone());
				assert!(!accepts(kind, spec), "{}: {} = {value}", kind.kind.name(), field.name);
			}
		}
	}
}

#[test]
fn every_choice_is_taken_and_only_a_choice_or_a_list_has_them() {
	for kind in CATALOG {
		for field in kind.fields {
			let listed = !field.choices.is_empty();
			match field.type_ {
				Type::Choice => assert!(listed, "{}", field.name),
				Type::List => {}
				_ => assert!(!listed, "{}", field.name),
			}
			for choice in field.choices {
				let value = if field.type_ == Type::List { json!([choice]) } else { json!(choice) };
				let spec = with(&example(kind), field.name, value.clone());
				assert!(accepts(kind, spec), "{}: {} = {value}", kind.kind.name(), field.name);
			}
			if field.type_ == Type::List && listed {
				let outside = with(&example(kind), field.name, json!(["none of them"]));
				assert!(!accepts(kind, outside), "{}: {}", kind.kind.name(), field.name);
				let twice = json!([field.choices[0], field.choices[0]]);
				assert!(!accepts(kind, with(&example(kind), field.name, twice)), "{}", field.name);
			}
		}
	}
}

/// A node keeps a key the catalog does not name as given, and an app refuses one; neither checks
/// a key the catalog leaves out.
#[test]
fn a_key_outside_the_catalog_is_kept_by_a_node_and_refused_by_an_app() {
	for kind in CATALOG {
		let example = example(kind);
		let named = |key: &str| kind.fields.iter().any(|field| field.name == key);
		let mut others: Vec<&str> =
			example.as_object().unwrap().keys().map(String::as_str).filter(|key| !named(key)).collect();
		others.push("unnamed");
		for key in others {
			for value in [json!(null), json!("x"), json!(1), json!([]), json!({})] {
				let spec = with(&example, key, value.clone());
				let kept = kind.kind == Kind::Node;
				assert_eq!(accepts(kind, spec), kept, "{}: {key} = {value}", kind.kind.name());
			}
		}
	}
}

#[test]
fn names_are_unique_and_every_column_a_field() {
	let mut kinds: Vec<&str> = CATALOG.iter().map(|kind| kind.kind.name()).collect();
	kinds.sort_unstable();
	kinds.dedup();
	assert_eq!(kinds.len(), CATALOG.len());
	for kind in CATALOG {
		let mut fields: Vec<&str> = kind.fields.iter().map(|field| field.name).collect();
		fields.sort_unstable();
		fields.dedup();
		assert_eq!(fields.len(), kind.fields.len(), "{}", kind.kind.name());
		for column in kind.columns {
			assert!(fields.contains(column), "{}: {column}", kind.kind.name());
		}
	}
}

#[test]
fn a_kind_is_answered_in_the_shape_the_console_reads() {
	let answered = serde_json::to_value(&APP).unwrap();
	assert_eq!(
		answered,
		json!({
			"name": "app",
			"fields": [
				{ "name": "placements", "type": "list", "editable": true, "required": true },
				{ "name": "release", "type": "object", "editable": true, "required": false },
			],
			"actions": [],
			"columns": ["placements"],
		})
	);
	let tier = serde_json::to_value(&NODE.fields[1]).unwrap();
	assert_eq!(tier["choices"], json!(["datacenter", "home", "transient"]));
}
