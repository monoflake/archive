//! The catalog of kinds, `GET /v1/kinds`: what the console draws a resource from, so a field added
//! here appears there without a change to it -- RFC cpc2b. The tests hold it to what each kind's
//! own module accepts.

use crate::node;
use serde::Serialize;

/// Every kind of resource the registry holds, answered by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
	Node,
	App,
	Account,
	Domain,
}

impl Kind {
	/// As it is answered, and as `changes.kind` holds it.
	pub const fn name(self) -> &'static str {
		match self {
			Self::Node => "node",
			Self::App => "app",
			Self::Account => "account",
			Self::Domain => "domain",
		}
	}

	/// Its collection's segment of a route, `/v1/<plural>`.
	pub const fn plural(self) -> &'static str {
		match self {
			Self::Node => "nodes",
			Self::App => "apps",
			Self::Account => "accounts",
			Self::Domain => "domains",
		}
	}
}

/// Whether `key` has a resource id's shape, 16 lowercase hexadecimal digits -- RFC 2kuvi. A route
/// reads such a key as an id, so no name may have it.
pub fn is_id(key: &str) -> bool {
	key.len() == 16 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A name every kind but `node` takes: one DNS label, lowercase letters, digits and inner hyphens,
/// as every service's is, and never an id's shape, which a route would read as an id.
pub fn label_is_valid(name: &str) -> bool {
	(1..=63).contains(&name.len())
		&& name.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
		&& !name.starts_with('-')
		&& !name.ends_with('-')
		&& !is_id(name)
}

/// Why a spec is not stored, said for whoever sent it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Refusal {
	/// A key the kind does not let be written.
	Forbidden(String),
	/// A key it does, holding what it cannot be.
	Invalid(String),
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
	/// The kind a `reference` names a resource of, by its id; none for every other field.
	#[serde(skip_serializing_if = "Option::is_none")]
	pub kind: Option<Kind>,
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
	/// The id of a resource of the field's `kind`, as a node's `account`, which the console draws as
	/// a choice among them -- RFC 2kuvi.
	Reference,
}

/// None yet: backing up or restarting comes with the controllers, RFC jln34.
#[derive(Debug, Serialize)]
pub enum Action {}

const fn field(name: &'static str, type_: Type, required: bool) -> Field {
	Field { name, type_, choices: &[], kind: None, editable: true, required }
}

const fn reference(name: &'static str, kind: Kind, required: bool) -> Field {
	Field { name, type_: Type::Reference, choices: &[], kind: Some(kind), editable: true, required }
}

const fn choice(name: &'static str, choices: &'static [&'static str], required: bool) -> Field {
	Field { name, type_: Type::Choice, choices, kind: None, editable: true, required }
}

/// A list of `choices`, each at most once.
const fn choices(name: &'static str, choices: &'static [&'static str], required: bool) -> Field {
	Field { name, type_: Type::List, choices, kind: None, editable: true, required }
}

pub const NODE: Description = Description {
	kind: Kind::Node,
	fields: &[
		field("address", Type::String, true),
		choice("tier", node::TIERS, true),
		reference("domain", Kind::Domain, true),
		choice("system", node::SYSTEMS, true),
		field("core", Type::Boolean, false),
		field("expiry", Type::Integer, false),
		field("retired", Type::Boolean, false),
		// RFC 32hd9's, beside the axes they are derived from until `nodes.toml` writes them.
		choice("hosting", node::HOSTINGS, false),
		reference("account", Kind::Account, false),
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

pub const ACCOUNT: Description = Description {
	kind: Kind::Account,
	fields: &[field("provider", Type::String, true), field("identities", Type::List, true)],
	actions: &[],
	columns: &["provider"],
};

pub const DOMAIN: Description = Description {
	kind: Kind::Domain,
	fields: &[field("note", Type::String, false)],
	actions: &[],
	columns: &["note"],
};

pub const CATALOG: &[Description] = &[NODE, APP, ACCOUNT, DOMAIN];

#[cfg(test)]
mod tests;
