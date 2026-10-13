use super::*;
use crate::{account, app};
use serde_json::{Value, json};

/// Whether `kind`'s validation stores `spec`.
fn accepts(kind: &Description, spec: Value) -> bool {
	match kind.kind {
		Kind::Node => node::record(spec).is_ok(),
		Kind::App => app::spec(spec).is_ok(),
		Kind::Account => account::spec(spec).is_ok(),
	}
}

/// A whole spec each kind stores, every field of the catalog in it; a node's keeps two keys more,
/// which it keeps as given.
fn example(kind: &Description) -> Value {
	match kind.kind {
		Kind::Node => json!({
			"address": "100.64.0.9",
			"tier": "datacenter",
			"system": "debian",
			"core": true,
			"expiry": 2036,
			"retired": false,
			"hosting": "datacenter",
			"account": "0123456789abcdef",
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
		Kind::Account => json!({
			"provider": "oracle",
			"identities": [{ "kind": "email", "value": "me@example.com" }],
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
		// Neither an id nor, during the expand, the code it held before.
		Type::Reference => vec![json!(1), json!(null), json!(["x"]), json!("0123456789ABCDEF")],
	}
}

/// An identity as the catalog describes it: its kind a choice, which is how an item is told
/// apart, and its value, required, kept and answered, and masked.
#[test]
fn an_identity_s_fields_are_what_an_account_takes() {
	let identities = ACCOUNT.fields.iter().find(|field| field.name == "identities").unwrap();
	let [kind, value] = identities.fields else { panic!("two fields") };
	assert_eq!((kind.type_, kind.choices, kind.required), (Type::Choice, account::IDENTITIES, true));
	let marks = (value.type_, value.required, value.masked, value.secret, value.editable);
	assert_eq!(marks, (Type::String, true, true, false, true));
	let taken = |identity: Value| {
		account::spec(json!({ "provider": "oracle", "identities": [identity] })).is_ok()
	};
	for choice in kind.choices {
		assert!(taken(json!({ "kind": choice, "value": "x" })), "{choice}");
		assert!(!taken(json!({ "kind": choice })), "{choice} without its value");
	}
	for wrong in [json!(1), json!(null), json!(""), json!("two\nlines")] {
		assert!(!taken(json!({ "kind": "email", "value": wrong.clone() })), "{wrong}");
	}
	let answered = serde_json::to_value(identities).unwrap();
	assert_eq!(
		answered["fields"],
		json!([
			{ "name": "kind", "type": "choice", "choices": ["user_id", "tenancy", "email"], "editable": true, "required": true },
			{ "name": "value", "type": "string", "masked": true, "editable": true, "required": true },
		])
	);
}

/// What a list of objects says of its items: an item is told apart by its first required choice.
#[test]
fn every_list_of_objects_names_an_identifying_choice_first() {
	for kind in CATALOG {
		for field in kind.fields.iter().filter(|field| !field.fields.is_empty()) {
			assert_eq!(field.type_, Type::List, "{}", field.name);
			let first = field.fields.iter().find(|item| item.required);
			assert_eq!(first.map(|item| item.type_), Some(Type::Choice), "{}", field.name);
			assert_eq!(field.fields[0].type_, Type::Choice, "{}: kept first", field.name);
		}
	}
}

/// Each kind answers its route's segment, so no reader derives it.
#[test]
fn each_kind_answers_its_plural() {
	for kind in CATALOG {
		assert_eq!(kind.plural, kind.kind.plural());
		let answered = serde_json::to_value(kind).unwrap();
		assert_eq!(answered["plural"], json!(kind.kind.plural()));
	}
	let plurals: Vec<&str> = CATALOG.iter().map(|kind| kind.plural).collect();
	assert_eq!(plurals, ["nodes", "apps", "accounts"]);
}

/// An account has no name, and the catalog says so; every other kind is named -- RFC 32hd9.
#[test]
fn a_kind_without_a_name_is_marked_so() {
	for kind in CATALOG {
		assert_eq!(kind.named, kind.kind.named());
		let answered = serde_json::to_value(kind).unwrap();
		assert_eq!(answered["named"], json!(kind.kind != Kind::Account), "{}", kind.kind.name());
	}
}

/// A reference names the kind it points at, and no other field names one.
#[test]
fn a_field_names_a_kind_exactly_where_it_is_a_reference() {
	for kind in CATALOG {
		for field in kind.fields {
			assert_eq!(field.kind.is_some(), field.type_ == Type::Reference, "{}", field.name);
		}
	}
	let account = NODE.fields.iter().find(|field| field.name == "account").unwrap();
	let answered = serde_json::to_value(account).unwrap();
	assert_eq!(
		answered,
		json!({ "name": "account", "type": "reference", "kind": "account", "editable": true, "required": false })
	);
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

/// A node keeps a key the catalog does not name as given, and every other kind refuses one; none
/// checks a key the catalog leaves out.
#[test]
fn a_key_outside_the_catalog_is_kept_by_a_node_and_refused_by_every_other_kind() {
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
			"plural": "apps",
			"named": true,
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

/// The committed `file`, beside the crate, holds what `made` makes now, so a type or a value
/// changed without it fails here, where it changed -- RFC bq3vn. `REGENERATE=1` writes it instead.
fn committed(file: &str, made: &str) {
	let path = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
	if std::env::var("REGENERATE").is_ok_and(|value| value == "1") {
		std::fs::write(path, made).unwrap();
		return;
	}
	let committed = std::fs::read_to_string(path).unwrap_or_default();
	let held = |text: &str| serde_json::from_str::<Value>(text).ok();
	assert!(
		held(&committed).is_some() && held(&committed) == held(made),
		"apps/data/registry/{file} is not what the catalog makes; write it again with \
		 `REGENERATE=1 cargo test -p registry-service committed_catalog` and commit it"
	);
}

#[test]
fn committed_catalog_schema() {
	committed("catalog.schema.json", &schema());
}

#[test]
fn committed_catalog_json() {
	committed("catalog.json", &catalog());
}

#[test]
fn committed_catalog_answer_example() {
	committed("examples/catalog/valid/answer.json", &answer());
}
