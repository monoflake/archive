//! What changed between two versions of a node's snapshot, and the digest that says two relays hold
//! the same one. A delta names the version it applies to and carries the digest of its result, so a
//! relay holding another version, or reading another digest, asks for the node whole instead. See
//! spec/architecture/relay.md, RFC grmtb.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// The digest's name, which both ends of a connection must say alike in their first message: it
/// names the hash and the form it is taken over, so a change to either is a new name.
pub const DIGEST: &str = "xxh3-64";

/// A snapshot's digest: 64 bits of xxh3 over its JSON written compact. A `Value`'s keys are sorted
/// -- serde_json keeps a map ordered unless `preserve_order` is asked for, and nothing here asks --
/// so the same snapshot is the same text wherever it is written.
pub fn digest(snapshot: &Value) -> u64 {
	let text = serde_json::to_vec(snapshot).expect("a snapshot is a tree of strings");
	xxhash_rust::xxh3::xxh3_64(&text)
}

/// A digest as the wire carries it: sixteen hex digits.
pub fn hex(digest: u64) -> String {
	format!("{digest:016x}")
}

/// What changed of a snapshot's keys: each set whole, each gone named, and its `apps` apart, as
/// rows by name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Changes {
	#[serde(default, skip_serializing_if = "Map::is_empty")]
	pub set: Map<String, Value>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub unset: Vec<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub apps: Option<Rows>,
}

/// What changed of a list of rows keyed by `name`: rows new or changed, names gone, and the names
/// in their new order where it is not the old one's with those applied.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Rows {
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub set: Vec<Value>,
	#[serde(default, skip_serializing_if = "Vec::is_empty")]
	pub unset: Vec<String>,
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub order: Option<Vec<String>>,
}

impl Changes {
	pub fn is_empty(&self) -> bool {
		self.set.is_empty() && self.unset.is_empty() && self.apps.is_none()
	}
}

const APPS: &str = "apps";

fn name(row: &Value) -> Option<&str> {
	row.get("name").and_then(Value::as_str)
}

/// What takes `base` to `next`, both objects; `apps` diffed by name where both hold a list of
/// named rows, and set whole otherwise.
pub fn diff(base: &Value, next: &Value) -> Changes {
	let empty = Map::new();
	let (base, next) = (base.as_object().unwrap_or(&empty), next.as_object().unwrap_or(&empty));
	let mut changes = Changes::default();
	for (key, value) in next {
		if base.get(key) == Some(value) {
			continue;
		}
		let rows = (key == APPS).then(|| rows(base.get(key), value)).flatten();
		match rows {
			Some(rows) => changes.apps = Some(rows),
			None => {
				changes.set.insert(key.clone(), value.clone());
			}
		}
	}
	changes.unset = base.keys().filter(|key| !next.contains_key(*key)).cloned().collect();
	changes
}

/// The rows that take `base` to `next`, or none where either is not a list of named rows.
fn rows(base: Option<&Value>, next: &Value) -> Option<Rows> {
	let (base, next) = (base?.as_array()?, next.as_array()?);
	let (before, after) = (names(base)?, names(next)?);
	let mut rows = Rows::default();
	rows.unset =
		before.iter().filter(|old| !after.contains(old)).map(|old| (*old).to_owned()).collect();
	for row in next {
		let held = base.iter().find(|old| name(old) == name(row));
		if held != Some(row) {
			rows.set.push(row.clone());
		}
	}
	let placed = placed(&before, &rows);
	if placed != after {
		rows.order = Some(after.iter().map(|one| (*one).to_owned()).collect());
	}
	Some(rows)
}

/// Each row's name, or none where a row has none.
fn names(rows: &[Value]) -> Option<Vec<&str>> {
	rows.iter().map(name).collect()
}

/// The names `before` holds once `rows` is applied without an order: those gone taken out, those
/// new put at the end.
fn placed<'a>(before: &[&'a str], rows: &'a Rows) -> Vec<&'a str> {
	let mut names: Vec<&str> =
		before.iter().copied().filter(|old| !rows.unset.iter().any(|gone| gone == old)).collect();
	for row in &rows.set {
		if let Some(new) = name(row).filter(|new| !names.contains(new)) {
			names.push(new);
		}
	}
	names
}

/// `base` with `changes` applied, or none where `base` is no object or its apps no list of named
/// rows the changes can apply to.
pub fn apply(base: &Value, changes: &Changes) -> Option<Value> {
	let mut next = base.as_object()?.clone();
	for key in &changes.unset {
		next.remove(key);
	}
	for (key, value) in &changes.set {
		next.insert(key.clone(), value.clone());
	}
	if let Some(rows) = &changes.apps {
		let held = next.get(APPS).and_then(Value::as_array).cloned().unwrap_or_default();
		next.insert(APPS.to_owned(), Value::Array(apply_rows(&held, rows)?));
	}
	Some(Value::Object(next))
}

fn apply_rows(base: &[Value], rows: &Rows) -> Option<Vec<Value>> {
	let mut next: Vec<Value> = base
		.iter()
		.filter(|row| !rows.unset.iter().any(|gone| Some(gone.as_str()) == name(row)))
		.cloned()
		.collect();
	for row in &rows.set {
		let named = name(row)?;
		match next.iter_mut().find(|held| name(held) == Some(named)) {
			Some(held) => *held = row.clone(),
			None => next.push(row.clone()),
		}
	}
	if let Some(order) = &rows.order {
		let mut ordered = Vec::with_capacity(order.len());
		for named in order {
			ordered.push(next.iter().find(|row| name(row) == Some(named.as_str()))?.clone());
		}
		next = ordered;
	}
	Some(next)
}

#[cfg(test)]
mod tests {
	use super::*;
	use serde_json::json;

	fn app(name: &str, image: &str) -> Value {
		json!({ "name": name, "image": image, "running": true })
	}

	#[test]
	fn a_changed_app_travels_as_its_row_alone_and_applies_to_the_same_snapshot() {
		let base = json!({ "taken_at": "1", "apps": [app("geo", "geo:1"), app("apt", "apt:1")],
			"machine": { "info": { "cores": 2 } }, "stale": ["machine"] });
		let next = json!({ "taken_at": "2", "apps": [app("geo", "geo:2"), app("apt", "apt:1")],
			"machine": { "info": { "cores": 2 } }, "alarms": { "cpu": { "since": "x" } } });
		let changes = diff(&base, &next);
		assert_eq!(changes.apps, Some(Rows { set: vec![app("geo", "geo:2")], ..Rows::default() }));
		assert_eq!(changes.unset, ["stale"]);
		assert_eq!(changes.set.keys().collect::<Vec<_>>(), ["alarms", "taken_at"]);
		let applied = apply(&base, &changes).unwrap();
		assert_eq!(applied, next);
		assert_eq!(digest(&applied), digest(&next));
	}

	#[test]
	fn rows_gone_new_and_moved_keep_the_origins_order() {
		let base = json!({ "apps": [app("a", "1"), app("b", "1"), app("c", "1")] });
		let next = json!({ "apps": [app("c", "1"), app("d", "1"), app("a", "2")] });
		let changes = diff(&base, &next);
		let rows = changes.apps.clone().unwrap();
		assert_eq!(rows.unset, ["b"]);
		assert_eq!(rows.order.as_deref(), Some(&["c".to_owned(), "d".into(), "a".into()][..]));
		assert_eq!(apply(&base, &changes).unwrap(), next);
		// A row added at the end needs no order.
		let grown = json!({ "apps": [app("a", "1"), app("b", "1"), app("c", "1"), app("e", "1")] });
		assert_eq!(diff(&base, &grown).apps.unwrap().order, None);
	}

	#[test]
	fn nothing_changed_is_an_empty_delta_and_the_digest_names_the_snapshot() {
		let snapshot = json!({ "apps": [app("geo", "geo:1")], "taken_at": "1" });
		assert!(diff(&snapshot, &snapshot).is_empty());
		// Written in another order, it is the same snapshot.
		let reordered: Value = serde_json::from_str(
			r#"{ "taken_at": "1", "apps": [
			{ "running": true, "image": "geo:1", "name": "geo" }] }"#,
		)
		.unwrap();
		assert_eq!(digest(&reordered), digest(&snapshot));
		assert_ne!(digest(&json!({ "taken_at": "2" })), digest(&snapshot));
		assert_eq!(hex(0xab), "00000000000000ab");
	}
}
