//! The catalog of kinds, `GET /v1/kinds`: what the console draws a resource from, so a field added
//! here appears there without a change to it -- RFC cpc2b. The tests hold it to what node.rs and
//! app.rs accept.

use crate::node;
use serde::Serialize;

/// Every kind of resource the registry holds, answered by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
	Node,
	App,
}

impl Kind {
	/// As it is answered, and as `changes.kind` holds it.
	pub const fn name(self) -> &'static str {
		match self {
			Self::Node => "node",
			Self::App => "app",
		}
	}
}

/// Whether `key` has a resource id's shape, 16 lowercase hexadecimal digits -- RFC 2kuvi. A route
/// reads such a key as an id, so no name may have it.
pub fn is_id(key: &str) -> bool {
	key.len() == 16 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// One kind as the catalog describes it.
#[derive(Debug, Serialize)]
pub struct Description {
	#[serde(rename = "name")]
	pub kind: Kind,
	pub fields: &'static [Field],
	pub actions: &'static [Action],
	/// The fields a list shows first, in order.
	pub columns: &'static [&'static str],
}

#[derive(Debug, Serialize)]
pub struct Field {
	pub name: &'static str,
	#[serde(rename = "type")]
	pub type_: Type,
	/// What a `choice` may be, or each item of a `list` of choices, each once; empty for every other
	/// field.
	#[serde(skip_serializing_if = "<[_]>::is_empty")]
	pub choices: &'static [&'static str],
	pub editable: bool,
	pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Type {
	String,
	Integer,
	Boolean,
	Choice,
	List,
	/// A value of named keys of its own, as an app's `release`; app.rs holds its shape.
	Object,
}

/// None yet: backing up or restarting comes with the controllers, RFC jln34.
#[derive(Debug, Serialize)]
pub enum Action {}

const fn field(name: &'static str, type_: Type, required: bool) -> Field {
	Field { name, type_, choices: &[], editable: true, required }
}

const fn choice(name: &'static str, choices: &'static [&'static str], required: bool) -> Field {
	Field { name, type_: Type::Choice, choices, editable: true, required }
}

/// A list of `choices`, each at most once.
const fn choices(name: &'static str, choices: &'static [&'static str], required: bool) -> Field {
	Field { name, type_: Type::List, choices, editable: true, required }
}

pub const NODE: Description = Description {
	kind: Kind::Node,
	fields: &[
		field("address", Type::String, true),
		choice("tier", node::TIERS, true),
		field("domain", Type::String, true),
		choice("system", node::SYSTEMS, true),
		field("core", Type::Boolean, false),
		field("expiry", Type::Integer, false),
		field("retired", Type::Boolean, false),
		// RFC 32hd9's, beside the axes they are derived from until `nodes.toml` writes them.
		choice("hosting", node::HOSTINGS, false),
		field("account", Type::String, false),
		choices("roles", node::ROLES, false),
	],
	actions: &[],
	columns: &["address", "tier", "domain", "system"],
};

pub const APP: Description = Description {
	kind: Kind::App,
	fields: &[field("placements", Type::List, true), field("release", Type::Object, false)],
	actions: &[],
	columns: &["placements"],
};

pub const CATALOG: &[Description] = &[NODE, APP];

#[cfg(test)]
mod tests;
