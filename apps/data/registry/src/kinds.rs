//! The catalog of kinds, `GET /v1/kinds`: what the console draws a resource from, so a field added
//! here appears there without a change to it -- RFC cpc2b. The tests hold it to what each kind's
//! own module accepts.

use crate::{account, node};
use schemars::JsonSchema;
use serde::Serialize;

/// Every kind of resource the registry holds, answered by its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
	Node,
	App,
	Account,
}

impl Kind {
	/// As it is answered, and as `changes.kind` holds it.
	pub const fn name(self) -> &'static str {
		match self {
			Self::Node => "node",
			Self::App => "app",
			Self::Account => "account",
		}
	}

	/// Its collection's segment of a route, `/v1/<plural>`.
	pub const fn plural(self) -> &'static str {
		match self {
			Self::Node => "nodes",
			Self::App => "apps",
			Self::Account => "accounts",
		}
	}

	/// Whether its resources have a name: an account has none, and is its id -- RFC 32hd9.
	pub const fn named(self) -> bool {
		!matches!(self, Self::Account)
	}
}

/// Whether `key` has a resource id's shape, 16 lowercase hexadecimal digits -- RFC 2kuvi. A route
/// reads such a key as an id, so no name may have it.
pub fn is_id(key: &str) -> bool {
	key.len() == 16 && key.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A name every named kind but `node` takes: one DNS label, lowercase letters, digits and inner
/// hyphens, as every service's is, and never an id's shape, which a route would read as an id.
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
#[derive(Debug, Serialize, JsonSchema)]
pub struct Description {
	#[serde(rename = "name")]
	pub kind: Kind,
	/// Its collection's segment of a route, `/v1/<plural>`, so no reader derives one.
	pub plural: &'static str,
	/// False for a kind whose resources have no name, an account's: each is its id, made by a `POST`
	/// to its collection, and shown by its fields and its id's first eight digits -- RFC 32hd9.
	pub named: bool,
	pub fields: &'static [Field],
	pub actions: &'static [Action],
	/// The fields a list shows first, in order.
	pub columns: &'static [&'static str],
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct Field {
	pub name: &'static str,
	#[serde(rename = "type")]
	pub type_: Type,
	/// What a `choice` may be, or each item of a `list` of choices, each once; empty for every other
	/// field.
	#[serde(skip_serializing_if = "<[_]>::is_empty")]
	pub choices: &'static [&'static str],
	/// The kind a `reference` names a resource of, by its id; absent, never null, for every other
	/// field.
	#[serde(skip_serializing_if = "Option::is_none")]
	#[schemars(with = "Kind")]
	pub kind: Option<Kind>,
	/// The fields of each item of a `list` of objects, as an account's `identities`, in this same
	/// shape; an item's identifying field is its first required `choice`. Empty for every other
	/// field.
	#[serde(skip_serializing_if = "<[_]>::is_empty")]
	pub fields: &'static [Field],
	/// Written and never read back, its value handed to the secret service and answered by the name
	/// it is kept under; beside its ordinary `type`. No field is one today.
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub secret: bool,
	/// Kept and answered like any other value, and drawn hidden until the reader asks to see it, as
	/// an identity's `value` -- RFC cpc2b; beside its ordinary `type`.
	#[serde(skip_serializing_if = "std::ops::Not::not")]
	pub masked: bool,
	/// False for what only the registry writes.
	pub editable: bool,
	pub required: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, JsonSchema)]
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
#[derive(Debug, Serialize, JsonSchema)]
pub enum Action {}

const fn field(name: &'static str, type_: Type, required: bool) -> Field {
	Field {
		name,
		type_,
		choices: &[],
		kind: None,
		fields: &[],
		secret: false,
		masked: false,
		editable: true,
		required,
	}
}

const fn reference(name: &'static str, kind: Kind, required: bool) -> Field {
	Field {
		name,
		type_: Type::Reference,
		choices: &[],
		kind: Some(kind),
		fields: &[],
		secret: false,
		masked: false,
		editable: true,
		required,
	}
}

const fn choice(name: &'static str, choices: &'static [&'static str], required: bool) -> Field {
	Field {
		name,
		type_: Type::Choice,
		choices,
		kind: None,
		fields: &[],
		secret: false,
		masked: false,
		editable: true,
		required,
	}
}

/// A list of `choices`, each at most once.
const fn choices(name: &'static str, choices: &'static [&'static str], required: bool) -> Field {
	Field {
		name,
		type_: Type::List,
		choices,
		kind: None,
		fields: &[],
		secret: false,
		masked: false,
		editable: true,
		required,
	}
}

pub const NODE: Description = Description {
	kind: Kind::Node,
	plural: Kind::Node.plural(),
	named: Kind::Node.named(),
	fields: &[
		field("address", Type::String, true),
		choice("tier", node::TIERS, true),
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
	columns: &["address", "tier", "account", "system"],
};

pub const APP: Description = Description {
	kind: Kind::App,
	plural: Kind::App.plural(),
	named: Kind::App.named(),
	fields: &[
		field("placements", Type::List, true),
		field("release", Type::Object, false),
		field("display_name", Type::String, false),
		field("repository", Type::String, false),
	],
	actions: &[],
	columns: &["display_name", "placements"],
};

/// An account's identity: its kind, and its value, kept and answered, drawn hidden until asked for.
const IDENTITY: &[Field] = &[
	choice("kind", account::IDENTITIES, true),
	Field { masked: true, ..field("value", Type::String, true) },
];

pub const ACCOUNT: Description = Description {
	kind: Kind::Account,
	plural: Kind::Account.plural(),
	named: Kind::Account.named(),
	fields: &[
		choice("provider", account::PROVIDERS, true),
		Field { fields: IDENTITY, ..field("identities", Type::List, true) },
	],
	actions: &[],
	columns: &["provider"],
};

pub const CATALOG: &[Description] = &[NODE, APP, ACCOUNT];

/// What `GET /v1/kinds` answers inside the envelope: every kind the registry holds. Its JSON
/// Schema is `catalog.schema.json`, made from this type and committed, which every reader in
/// another language is derived from -- RFC bq3vn and the workspace's spec/json.md.
#[derive(Debug, Serialize, JsonSchema)]
pub struct Catalog {
	pub items: &'static [Description],
}

pub const ANSWER: Catalog = Catalog { items: CATALOG };

/// The schema as written to `catalog.schema.json`. As the answer is written, not read: a field
/// left out when empty is not required.
pub fn schema() -> String {
	let settings = schemars::generate::SchemaSettings::draft2020_12().for_serialize();
	written(&settings.into_generator().into_root_schema_for::<Catalog>())
}

/// The kinds as written to `catalog.json`, what `GET /v1/kinds` answers as its `items`: a reader's
/// tests load it whole, values included, rather than keep a copy of their own -- RFC bq3vn.
pub fn catalog() -> String {
	written(&CATALOG)
}

/// What `GET /v1/kinds` answers inside the envelope, as written to the catalog's first valid
/// example, `examples/catalog/valid/answer.json`, which readers in other languages test against.
pub fn answer() -> String {
	written(&ANSWER)
}

/// `value` indented with tabs and ending in a newline, as `mise run fmt` leaves JSON; a check still
/// compares what a file holds, never its text, so the formatter owns its layout.
fn written(value: &impl Serialize) -> String {
	let mut written = Vec::new();
	let formatter = serde_json::ser::PrettyFormatter::with_indent(b"\t");
	let mut serializer = serde_json::Serializer::with_formatter(&mut written, formatter);
	value.serialize(&mut serializer).expect("the catalog is JSON");
	String::from_utf8(written).expect("JSON is UTF-8") + "\n"
}

#[cfg(test)]
mod tests;
