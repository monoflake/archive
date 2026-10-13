//! What an app's name and spec have to be before either is stored. An app's spec holds only what
//! the registry keeps of it, so any other key is refused rather than kept. See RFC ja5hm and
//! RFC cpc2b.

use crate::kinds;
pub use crate::kinds::Refusal;
use crate::node;
use serde_json::{Map, Value};

/// One DNS label, as every service's `name` is; see `kinds::label_is_valid`.
pub fn name_is_valid(name: &str) -> bool {
	kinds::label_is_valid(name)
}

/// Every key an app's spec may hold.
const FIELDS: &[&str] = &["placements", "release"];

/// Every key a `release` holds, each required.
const RELEASE: &[&str] = &["repository", "run", "commit"];

/// The spec as stored: `placements`, the nodes it runs on, each named once and in the order given,
/// and `release`, what it runs there, absent until one is recorded -- RFC jln34.
pub fn spec(spec: Value) -> Result<Map<String, Value>, Refusal> {
	let Value::Object(spec) = spec else {
		return Err(Refusal::Invalid("an app's spec is a JSON object".into()));
	};
	if let Some(key) = spec.keys().find(|key| !FIELDS.contains(&key.as_str())) {
		return Err(Refusal::Forbidden(format!("`{key}` is not a field of an app")));
	}
	if let Some(message) = spec.get("release").and_then(release_refusal) {
		return Err(Refusal::Invalid(message));
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

/// What is wrong with a `release`, if anything: the run of a repository that built it, and the
/// commit it built, as GitHub names each.
fn release_refusal(release: &Value) -> Option<String> {
	let Value::Object(release) = release else {
		return Some("`release` is an object of `repository`, `run` and `commit`".into());
	};
	if let Some(key) = release.keys().find(|key| !RELEASE.contains(&key.as_str())) {
		return Some(format!("`release` has no `{key}`"));
	}
	if let Some(key) = RELEASE.iter().find(|key| !release.contains_key(**key)) {
		return Some(format!("`release.{key}` is required"));
	}
	if !release["repository"].as_str().is_some_and(repository_is_valid) {
		return Some("`release.repository` is a GitHub repository, as `owner/repo`".into());
	}
	if !release["run"].as_u64().is_some_and(|run| run > 0) {
		return Some("`release.run` is a run's number, a whole number above 0".into());
	}
	if !release["commit"].as_str().is_some_and(commit_is_valid) {
		return Some("`release.commit` is a commit's 40 lowercase hexadecimal digits".into());
	}
	None
}

/// `owner/repo`, each as GitHub allows: letters, digits, `-`, `_` and `.`.
fn repository_is_valid(repository: &str) -> bool {
	let named = |part: &str| {
		!part.is_empty() && part.bytes().all(|b| b.is_ascii_alphanumeric() || b"-_.".contains(&b))
	};
	repository.split_once('/').is_some_and(|(owner, repo)| named(owner) && named(repo))
}

fn commit_is_valid(commit: &str) -> bool {
	commit.len() == 40 && commit.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
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
		// An id's shape is an id, never a name; one digit either way is a name again.
		let id = "0123456789abcdef";
		assert!(!name_is_valid(id));
		assert!(name_is_valid(&id[1..]));
		assert!(name_is_valid(&format!("{id}0")));
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
		for key in ["status", "drift", "image", "releases"] {
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

	fn released(release: Value) -> Value {
		json!({ "placements": ["tyo"], "release": release })
	}

	fn release() -> Value {
		json!({
			"repository": "monoflake/platform",
			"run": 18_000_000_001_u64,
			"commit": "0123456789abcdef0123456789abcdef01234567",
		})
	}

	#[test]
	fn a_release_is_kept_whole() {
		assert_eq!(spec(released(release())).map(Value::Object), Ok(released(release())));
		let mut dotted = release();
		dotted["repository"] = json!("canmi21/canmi.net");
		assert!(spec(released(dotted)).is_ok());
	}

	#[test]
	fn a_release_of_any_other_shape_is_refused_naming_the_field() {
		let with = |key: &str, value: Value| {
			let mut release = release();
			release[key] = value;
			release
		};
		let without = |key: &str| {
			let mut release = release();
			release.as_object_mut().unwrap().remove(key);
			release
		};
		let wrong = [
			(json!("monoflake/platform@0123"), "`release` is"),
			(json!(null), "`release` is"),
			(with("branch", json!("main")), "`release` has no `branch`"),
			(without("repository"), "`release.repository` is required"),
			(without("run"), "`release.run` is required"),
			(without("commit"), "`release.commit` is required"),
			(with("repository", json!("platform")), "`release.repository`"),
			(with("repository", json!("/platform")), "`release.repository`"),
			(with("repository", json!("mono flake/platform")), "`release.repository`"),
			(with("repository", json!("a/b/c")), "`release.repository`"),
			(with("run", json!(0)), "`release.run`"),
			(with("run", json!(-3)), "`release.run`"),
			(with("run", json!(1.5)), "`release.run`"),
			(with("run", json!("7")), "`release.run`"),
			(with("commit", json!("0123456789ABCDEF0123456789ABCDEF01234567")), "`release.commit`"),
			(with("commit", json!("0123456789abcdef")), "`release.commit`"),
			(with("commit", json!("g123456789abcdef0123456789abcdef01234567")), "`release.commit`"),
		];
		for (release, named) in wrong {
			let Err(Refusal::Invalid(message)) = spec(released(release.clone())) else {
				panic!("{release} is taken");
			};
			assert!(message.starts_with(named), "{release}: {message}");
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
