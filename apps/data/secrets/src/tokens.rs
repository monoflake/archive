//! Who may ask what: the write token, the token the cores show each other, and each node's own read
//! token, which this service keeps only as a SHA-256 digest, so what it holds reads no node's
//! secrets. A node's read token is the HMAC of a fixed message keyed by its host's token, which
//! host and `mise run seal token` both derive; this service never sees either. See RFC 231yq.

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

/// Each node's read token, by the node's name, as the digest of it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct NodeTokens(BTreeMap<String, [u8; 32]>);

impl NodeTokens {
	/// `SECRETS_NODE_TOKENS` read: `node:digest` pairs separated by whitespace, the digest the
	/// token's SHA-256 in lowercase hex. The first pair that is not one is the error.
	pub fn parse(value: &str) -> Result<Self, String> {
		value
			.split_whitespace()
			.map(|pair| {
				let (node, digest) = pair.split_once(':').ok_or_else(|| pair.to_owned())?;
				let digest = hex::decode(digest)
					.ok()
					.and_then(|digest| <[u8; 32]>::try_from(digest).ok())
					.ok_or_else(|| pair.to_owned())?;
				if crate::api::node_name_is_valid(node) {
					Ok((node.to_owned(), digest))
				} else {
					Err(pair.to_owned())
				}
			})
			.collect::<Result<_, _>>()
			.map(NodeTokens)
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

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn a_node_is_admitted_by_its_own_token_alone() {
		let value =
			format!("tyo:{} sin:{}", hex::encode(digest("tyo-token")), hex::encode(digest("sin-token")));
		let tokens = NodeTokens::parse(&value).unwrap();
		assert_eq!(tokens.len(), 2);
		assert!(tokens.admits("tyo", "tyo-token"));
		assert!(!tokens.admits("tyo", "sin-token"));
		assert!(!tokens.admits("tyo", ""));
		assert!(!tokens.admits("rdu", "tyo-token"));
		assert!(!tokens.admits("rdu", ""));
	}

	#[test]
	fn a_pair_that_is_not_one_is_refused() {
		let good = hex::encode(digest("x"));
		for value in
			["tyo".to_owned(), "tyo:abcd".into(), format!("TYO:{good}"), format!("tyo:{good}x")]
		{
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
		assert!(NodeTokens::parse(&format!("rdu:{digest}")).unwrap().admits("rdu", read));
	}

	#[test]
	fn equal_is_equality() {
		assert!(equal(b"abc", b"abc"));
		assert!(!equal(b"abc", b"abd"));
		assert!(!equal(b"abc", b"ab"));
		assert!(equal(b"", b""));
	}
}
