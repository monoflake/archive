//! The ledger's own codes, declared in `codes.json` with the status and the message each answers
//! with; lib's catalog keeps only the codes any API has. See the workspace's spec/json.md, "An
//! error's code is the producer's too".

use axum::http::StatusCode;
use axum::response::Response;
use serde::Deserialize;
use std::collections::BTreeMap;
use std::sync::OnceLock;

#[derive(Deserialize)]
struct Declared {
	status: u16,
	message: String,
}

fn declared() -> &'static BTreeMap<String, Declared> {
	static CODES: OnceLock<BTreeMap<String, Declared>> = OnceLock::new();
	CODES.get_or_init(|| serde_json::from_str(include_str!("../codes.json")).unwrap_or_default())
}

/// A failure under one of the ledger's codes, with its declared status and message.
pub fn failure(code: &str) -> Response {
	let message = declared().get(code).map_or(code, |declared| declared.message.as_str());
	failure_with(code, message)
}

/// The same, with a message more exact than the declared one. A code not declared answers `500`,
/// and the tests hold every code the sources name to `codes.json`.
pub fn failure_with(code: &str, message: impl ToString) -> Response {
	let status = declared()
		.get(code)
		.and_then(|declared| StatusCode::from_u16(declared.status).ok())
		.unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
	response::failure_with(status, code, message)
}

/// Every code a source answers with through this module -- `codes::failure("code")` and
/// `codes::failure_with("code", ...)` -- as `response::codes_named` finds lib's.
#[cfg(test)]
pub(crate) fn named(source: &str) -> Vec<&str> {
	source
		.split("codes::failure")
		.skip(1)
		.filter_map(|after| {
			let rest = after.strip_prefix("_with").unwrap_or(after).strip_prefix('(')?;
			rest.trim_start().strip_prefix('"')?.split('"').next()
		})
		.collect()
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn every_declared_code_is_the_ledgers_own_and_well_formed() {
		assert_eq!(declared().len(), 5, "codes.json did not parse whole");
		for (code, declared) in declared() {
			assert!(code.bytes().all(|b| b.is_ascii_lowercase() || b == b'_'), "{code}");
			assert!(response::message_of(code).is_none(), "{code} is lib's already");
			let status = StatusCode::from_u16(declared.status).unwrap();
			assert!(status.is_client_error() || status.is_server_error(), "{code}");
			assert!(declared.message.starts_with(|c: char| c.is_ascii_uppercase()), "{code}");
			assert!(!declared.message.ends_with('.'), "{code}");
		}
	}

	#[test]
	fn finds_the_codes_a_source_names() {
		let source = r#"codes::failure("no_such_run"), codes::failure_with(
			"result_too_large", "at most"), codes::failure(code), response::failure(x, "other")"#;
		assert_eq!(named(source), ["no_such_run", "result_too_large"]);
	}
}
