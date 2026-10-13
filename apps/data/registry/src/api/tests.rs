use super::*;
use crate::store::testing::{store, unreached};
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use serde_json::{Value, json};
use tower::ServiceExt;

pub(super) const TOKEN: &str = "s3cret";

pub(super) fn registry(store: Store, token: &str) -> Router {
	routes(Arc::new(Registry { store, token: token.into(), ledger: None }))
}

/// A router over a database never reached, for what is refused before the store is asked.
pub(super) fn refusing(token: &str) -> Router {
	registry(unreached(), token)
}

pub(super) async fn reached() -> Option<Router> {
	Some(registry(store().await?, TOKEN))
}

pub(super) fn sin() -> Value {
	json!({
		"address": "100.64.0.9",
		"tier": "datacenter",
		"domain": "oci-2",
		"system": "debian",
		"core": true,
		"expiry": 2036,
		"tunnel_via": "tyo",
	})
}

pub(super) async fn send(
	router: Router,
	method: &str,
	path: &str,
	bearer: Option<&str>,
	body: Option<Value>,
) -> (StatusCode, Value) {
	let mut request = Request::builder().method(method).uri(path);
	if let Some(bearer) = bearer {
		request = request.header("authorization", bearer);
	}
	let body = match body {
		Some(body) => {
			request = request.header("content-type", "application/json");
			Body::from(serde_json::to_vec(&body).unwrap())
		}
		None => Body::empty(),
	};
	let response = router.oneshot(request.body(body).unwrap()).await.unwrap();
	let status = response.status();
	let bytes = response.into_body().collect().await.unwrap().to_bytes();
	(status, serde_json::from_slice(&bytes).unwrap())
}

pub(super) async fn ask(router: Router, path: &str) -> (StatusCode, Value) {
	send(router, "GET", path, None, None).await
}

pub(super) fn code(answer: &(StatusCode, Value)) -> (StatusCode, &str) {
	(answer.0, answer.1["code"].as_str().unwrap_or_default())
}

/// Every write, each with a body it would otherwise take.
fn writes() -> [(&'static str, &'static str, Value); 4] {
	let app = json!({ "by": "test", "revision": 0, "spec": { "placements": ["tyo"] } });
	let node = json!({ "by": "test", "revision": 0, "spec": sin() });
	[
		("PUT", "/v1/nodes/sin", node.clone()),
		("PATCH", "/v1/nodes/sin", node),
		("PUT", "/v1/apps/web", app.clone()),
		("PATCH", "/v1/apps/web", app),
	]
}

#[tokio::test]
async fn writes_are_closed_without_a_token_whatever_the_bearer() {
	let router = refusing("");
	for (method, path, body) in writes() {
		for bearer in [None, Some("Bearer "), Some("Bearer s3cret")] {
			let answer = send(router.clone(), method, path, bearer, Some(body.clone())).await;
			let closed = (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable");
			assert_eq!(code(&answer), closed, "{method} {path}");
		}
	}
}

#[tokio::test]
async fn a_write_without_the_token_is_unauthorized() {
	let router = refusing(TOKEN);
	let wrong = ["Bearer s3cre", "Bearer s3cret!", "Bearer S3cret", "s3cret", "Basic s3cret"];
	for (method, path, body) in writes() {
		for bearer in std::iter::once(None).chain(wrong.map(Some)) {
			let answer = send(router.clone(), method, path, bearer, Some(body.clone())).await;
			let refused = (StatusCode::UNAUTHORIZED, "invalid_token");
			assert_eq!(code(&answer), refused, "{method} {path} {bearer:?}");
		}
	}
}

#[tokio::test]
async fn unhealthy_while_the_database_does_not_answer() {
	let router = refusing(TOKEN);
	let answer = ask(router.clone(), "/health").await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "store_unavailable"));
	assert_eq!(code(&ask(router, "/nope").await), (StatusCode::NOT_FOUND, "no_such_route"));
}

/// The routes before `/v1`, removed once no relay read them: each is no route at all now.
#[tokio::test]
async fn the_routes_before_v1_are_gone() {
	let router = refusing(TOKEN);
	let bearer = format!("Bearer {TOKEN}");
	let old = json!({ "by": "test", "node": sin() });
	let routes =
		[("GET", "/nodes"), ("PUT", "/nodes/sin"), ("DELETE", "/nodes/sin"), ("GET", "/changes")];
	for (method, path) in routes {
		let answer = send(router.clone(), method, path, Some(&bearer), Some(old.clone())).await;
		assert_eq!(code(&answer), (StatusCode::NOT_FOUND, "no_such_route"), "{method} {path}");
	}
}

#[test]
fn every_code_it_answers_with_is_in_the_catalogue() {
	let sources = [include_str!("../api.rs"), include_str!("v1.rs")];
	for code in sources.into_iter().flat_map(response::codes_named) {
		assert!(
			response::message_of(code).is_some(),
			"`{code}` is not in the response crate's codes.json"
		);
	}
}
