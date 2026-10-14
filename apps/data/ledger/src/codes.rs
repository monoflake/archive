//! The ledger's own codes, declared in `codes.json` with the status and the message each answers
//! with; lib's catalog keeps only the codes any API has. See the workspace's spec/json.md, "An
//! error's code is the producer's too".

use axum::response::Response;
use response::Codes;
use std::sync::OnceLock;

/// What `codes.json` declares, read once.
pub fn declared() -> &'static Codes {
	static CODES: OnceLock<Codes> = OnceLock::new();
	CODES.get_or_init(|| Codes::from_json(include_str!("../codes.json")).unwrap_or_default())
}

/// A failure under one of the ledger's codes, with its declared status and message.
pub fn failure(code: &str) -> Response {
	declared().failure(code)
}

/// The same, with a message more exact than the declared one. A code not declared answers `500`,
/// and the tests hold every code the sources name to `codes.json`.
pub fn failure_with(code: &str, message: impl ToString) -> Response {
	declared().failure_with(code, message)
}

#[cfg(test)]
mod tests {
	#[test]
	fn every_declared_code_is_the_ledgers_own_and_well_formed() {
		assert_eq!(super::declared().len(), 6, "codes.json did not parse whole");
		super::declared().assert_well_formed();
	}
}
