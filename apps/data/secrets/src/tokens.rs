//! Who may ask what: the write token, the tokens that each write one scope alone, the token the
//! cores show each other, and each node's own read token. A scope's and a node's are kept only as a
//! SHA-256 digest, so what this service holds writes and reads nothing. A node's read token is the
//! HMAC of a fixed message keyed by its host's token, which host and `mise run seal token` both
//! derive; this service never sees either. See RFC 231yq.

use axum::http::HeaderMap;
use axum::http::header::AUTHORIZATION;
use sha2::{Digest, Sha256};
use std::collections::BTreeMap;

/// The bearer a request carries, empty when it carries none.
pub fn bearer(headers: &HeaderMap) -> &str {
	headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.unwrap_or_default()
}

/// Whether two byte strings are equal, compared in constant time so the answer's timing says
/// nothing of how much of them matched.
pub fn equal(a: &[u8], b: &[u8]) -> bool {
	a.len() == b.len() && a.iter().zip(b).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

pub fn digest(token: &str) -> [u8; 32] {
	Sha256::digest(token.as_bytes()).into()
}

/// `key:digest` pairs separated by whitespace, each name one `valid` takes and each digest a
/// token's SHA-256 in lowercase hex. The first pair that is not one is the error.
fn pairs(value: &str, valid: fn(&str) -> bool) -> Result<Vec<(String, [u8; 32])>, String> {
	value
		.split_whitespace()
		.map(|pair| {
			let (name, digest) = pair.split_once(':').ok_or_else(|| pair.to_owned())?;
			let digest = hex::decode(digest)
				.ok()
				.and_then(|digest| <[u8; 32]>::try_from(digest).ok())
				.ok_or_else(|| pair.to_owned())?;
			if valid(name) { Ok((name.to_owned(), digest)) } else { Err(pair.to_owned()) }
		})
		.collect()
}

/// Each node's read token, by the node's id, as the digest of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeTokens(BTreeMap<String, [u8; 32]>);

impl NodeTokens {
	/// `SECRETS_NODE_TOKENS` read: `id:digest` pairs, see `pairs`.
	pub fn parse(value: &str) -> Result<Self, String> {
		Ok(NodeTokens(pairs(value, crate::api::node_id_is_valid)?.into_iter().collect()))
	}

	/// Whether `token` is `node`'s own. A node with no token admits nobody.
	pub fn admits(&self, node: &str, token: &str) -> bool {
		let given = digest(token);
		self.0.get(node).is_some_and(|kept| equal(&given, kept))
	}

	pub fn len(&self) -> usize {
		self.0.len()
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

/// What a writer's token reaches: every scope and app, `SECRETS_TOKEN`'s, or one app of one scope.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Grant {
	Every,
	Held { scope: String, app: String },
}

impl Grant {
	pub fn reaches(&self, scope: &str, app: &str) -> bool {
		match self {
			Grant::Every => true,
			Grant::Held { scope: own_scope, app: own_app } => own_scope == scope && own_app == app,
		}
	}

	/// The one scope and app it is held to; None when it reaches every one.
	pub fn held(&self) -> Option<(&str, &str)> {
		match self {
			Grant::Every => None,
			Grant::Held { scope, app } => Some((scope, app)),
		}
	}
}

/// `scope/app`, each one DNS label, as a scope token names what it writes.
fn scope_and_app(name: &str) -> Option<(&str, &str)> {
	let (scope, app) = name.split_once('/')?;
	(crate::api::label_is_valid(scope) && crate::api::label_is_valid(app)).then_some((scope, app))
}

/// The tokens that each write one app of one scope alone, as the digest of each.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ScopeTokens(Vec<(String, String, [u8; 32])>);

impl ScopeTokens {
	/// `SECRETS_SCOPE_TOKENS` read: `scope/app:digest` pairs, see `pairs`. A scope and app may be
	/// named twice, so its token is replaced with no moment none is admitted; one digest named for
	/// two is refused, since that token would write both.
	pub fn parse(value: &str) -> Result<Self, String> {
		let pairs = pairs(value, |name| scope_and_app(name).is_some())?;
		for (index, (name, digest)) in pairs.iter().enumerate() {
			if pairs[..index].iter().any(|(other, kept)| kept == digest && other != name) {
				return Err(format!("{name}:{}", hex::encode(digest)));
			}
		}
		let held = pairs
			.into_iter()
			.map(|(name, digest)| {
				let (scope, app) = scope_and_app(&name).expect("checked as each pair was read");
				(scope.to_owned(), app.to_owned(), digest)
			})
			.collect();
		Ok(ScopeTokens(held))
	}

	/// The scope and app `token` writes, if it is one of these. Every digest is compared whatever
	/// matched, so the time taken says nothing of which did.
	pub fn grant_of(&self, token: &str) -> Option<Grant> {
		let given = digest(token);
		let mut found = None;
		for (scope, app, kept) in &self.0 {
			if equal(&given, kept) && found.is_none() {
				found = Some(Grant::Held { scope: scope.clone(), app: app.clone() });
			}
		}
		found
	}

	pub fn len(&self) -> usize {
		self.0.len()
	}

	pub fn is_empty(&self) -> bool {
		self.0.is_empty()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	const TYO: &str = "d132b0f553e3dcc3";
	const SIN: &str = "9d399312d1c3ddc4";
	const RDU: &str = "d62c82b23a858876";

	#[test]
	fn a_node_is_admitted_by_its_own_token_alone() {
		let value = format!(
			"{TYO}:{} {SIN}:{}",
			hex::encode(digest("tyo-token")),
			hex::encode(digest("sin-token"))
		);
		let tokens = NodeTokens::parse(&value).unwrap();
		assert_eq!(tokens.len(), 2);
		assert!(tokens.admits(TYO, "tyo-token"));
		assert!(!tokens.admits(TYO, "sin-token"));
		assert!(!tokens.admits(TYO, ""));
		assert!(!tokens.admits(RDU, "tyo-token"));
		assert!(!tokens.admits(RDU, ""));
		assert!(!tokens.admits("tyo", "tyo-token"));
	}

	#[test]
	fn a_pair_that_is_not_one_is_refused() {
		let good = hex::encode(digest("x"));
		for value in [
			TYO.to_owned(),
			format!("{TYO}:abcd"),
			format!("tyo:{good}"),
			format!("D132B0F553E3DCC3:{good}"),
			format!("d132b0f553e3dcc:{good}"),
			format!("d132b0f553e3dcg3:{good}"),
			format!("{TYO}:{good}x"),
		] {
			assert!(NodeTokens::parse(&value).is_err(), "{value}");
		}
		assert!(NodeTokens::parse("").unwrap().is_empty());
	}

	/// The vector `.mise/tasks/seal_test.py` pins: a read token, as host derives it from its host
	/// token `host-s3cret`, and the digest `mise run seal token` writes for it.
	#[test]
	fn the_digest_the_task_writes_admits_the_token_host_derives() {
		let read = "e427d2347bcf34900e081847095ac6d30609ba546b08e680d5f8f4ee8c389116";
		let digest = "d0cd53f06568c9cdf0be664afcf1aa86acef7179e1840e1377afe84c902ba564";
		assert_eq!(hex::encode(super::digest(read)), digest);
		assert!(NodeTokens::parse(&format!("{RDU}:{digest}")).unwrap().admits(RDU, read));
	}

	#[test]
	fn a_scope_token_writes_its_own_app_of_its_own_scope_alone() {
		let value = format!(
			"accounts/registry:{} canmi/web:{} accounts/registry:{}",
			hex::encode(digest("accounts-token")),
			hex::encode(digest("canmi-token")),
			hex::encode(digest("accounts-next")),
		);
		let tokens = ScopeTokens::parse(&value).unwrap();
		assert_eq!(tokens.len(), 3);
		let registry = Grant::Held { scope: "accounts".into(), app: "registry".into() };
		assert_eq!(tokens.grant_of("accounts-token"), Some(registry.clone()));
		assert_eq!(tokens.grant_of("accounts-next"), Some(registry.clone()));
		assert_eq!(registry.held(), Some(("accounts", "registry")));
		assert_eq!(tokens.grant_of("platform-token"), None);
		assert_eq!(tokens.grant_of(""), None);
		assert!(registry.reaches("accounts", "registry"));
		assert!(!registry.reaches("accounts", "web") && !registry.reaches("canmi", "registry"));
		assert!(Grant::Every.reaches("canmi", "web") && Grant::Every.held().is_none());
	}

	#[test]
	fn a_scope_pair_that_is_not_one_is_refused() {
		let good = hex::encode(digest("x"));
		for value in [
			"accounts/registry".to_owned(),
			format!("accounts:{good}"),
			format!("accounts/:{good}"),
			format!("/registry:{good}"),
			format!("accounts/registry/x:{good}"),
			format!("Accounts/registry:{good}"),
			format!("accounts/-registry:{good}"),
			format!("accounts/registry:{good}0"),
			format!("accounts/registry:{good} accounts/web:{good}"),
		] {
			assert!(ScopeTokens::parse(&value).is_err(), "{value}");
		}
		assert!(ScopeTokens::parse("").unwrap().is_empty());
		assert!(
			ScopeTokens::parse(&format!("accounts/registry:{good} accounts/registry:{good}")).is_ok()
		);
	}

	#[test]
	fn equal_is_equality() {
		assert!(equal(b"abc", b"abc"));
		assert!(!equal(b"abc", b"abd"));
		assert!(!equal(b"abc", b"ab"));
		assert!(equal(b"", b""));
	}
}
