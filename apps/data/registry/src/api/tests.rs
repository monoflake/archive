use super::*;
use crate::store::testing::{store, unreached};
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

pub(super) const TOKEN: &str = "s3cret";

pub(super) fn registry(store: Store, token: &str) -> Router {
	routes(Arc::new(Registry { store, token: token.into() }))
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

async fn put_node(router: Router, name: &str, by: &str, node: Value) -> (StatusCode, Value) {
	let bearer = format!("Bearer {TOKEN}");
	let body = json!({ "by": by, "node": node });
	send(router, "PUT", &format!("/nodes/{name}"), Some(&bearer), Some(body)).await
}

async fn delete_node(router: Router, name: &str) -> (StatusCode, Value) {
	let bearer = format!("Bearer {TOKEN}");
	send(router, "DELETE", &format!("/nodes/{name}"), Some(&bearer), Some(json!({ "by": "test" })))
		.await
}

pub(super) async fn ask(router: Router, path: &str) -> (StatusCode, Value) {
	send(router, "GET", path, None, None).await
}

pub(super) fn code(answer: &(StatusCode, Value)) -> (StatusCode, &str) {
	(answer.0, answer.1["code"].as_str().unwrap_or_default())
}

#[tokio::test]
async fn writes_are_closed_without_a_token_whatever_the_bearer() {
	let router = refusing("");
	let body = json!({ "by": "test", "node": sin() });
	for bearer in [None, Some("Bearer "), Some("Bearer s3cret")] {
		let answer = send(router.clone(), "PUT", "/nodes/sin", bearer, Some(body.clone())).await;
		assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
		let answer =
			send(router.clone(), "DELETE", "/nodes/sin", bearer, Some(json!({ "by": "test" }))).await;
		assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
	}
}

#[tokio::test]
async fn a_write_without_the_token_is_unauthorized() {
	let router = refusing(TOKEN);
	let body = json!({ "by": "test", "node": sin() });
	let wrong = ["Bearer s3cre", "Bearer s3cret!", "Bearer S3cret", "s3cret", "Basic s3cret"];
	for bearer in std::iter::once(None).chain(wrong.map(Some)) {
		let answer = send(router.clone(), "PUT", "/nodes/sin", bearer, Some(body.clone())).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
		let answer =
			send(router.clone(), "DELETE", "/nodes/sin", bearer, Some(json!({ "by": "test" }))).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
	}
}

#[tokio::test]
async fn refuses_a_name_that_is_not_three_lowercase_letters() {
	let router = refusing(TOKEN);
	for name in ["SIN", "si", "sinn", "s1n"] {
		assert_eq!(
			code(&put_node(router.clone(), name, "test", sin()).await),
			(StatusCode::BAD_REQUEST, "invalid_name")
		);
		let answer = delete_node(router.clone(), name).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_name"));
	}
}

#[tokio::test]
async fn refuses_a_body_it_cannot_store() {
	let router = refusing(TOKEN);
	let bearer = format!("Bearer {TOKEN}");
	let mut tierless = sin();
	tierless.as_object_mut().unwrap().remove("tier");
	for body in [
		json!({ "node": sin() }),
		json!({ "by": "", "node": sin() }),
		json!({ "by": "  ", "node": sin() }),
		json!({ "by": "test" }),
		json!({ "by": "test", "node": [] }),
		json!({ "by": "test", "node": tierless }),
		json!([]),
	] {
		let answer = send(router.clone(), "PUT", "/nodes/sin", Some(&bearer), Some(body.clone())).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"), "{body}");
	}
	let answer = put_node(router.clone(), "sin", "test", json!({ "address": "x" })).await;
	assert_eq!(answer.1["message"], "`address` is the node's tailnet IPv4, a dotted quad");
	for body in [Some(json!({})), Some(json!({ "by": "" })), None] {
		let answer = send(router.clone(), "DELETE", "/nodes/sin", Some(&bearer), body).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"));
	}
}

#[tokio::test]
async fn unhealthy_while_the_database_does_not_answer() {
	let router = refusing(TOKEN);
	let answer = ask(router.clone(), "/health").await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "store_unavailable"));
	assert_eq!(code(&ask(router, "/nope").await), (StatusCode::NOT_FOUND, "no_such_route"));
}

#[test]
fn a_page_asked_is_forgiven_and_clamped() {
	let asked = |after: Option<&str>, limit: Option<&str>| {
		page_asked(&ChangesAsked { after: after.map(Into::into), limit: limit.map(Into::into) })
	};
	assert_eq!(asked(None, None), (0, DEFAULT_LIMIT));
	assert_eq!(asked(Some("12"), Some("5")), (12, 5));
	assert_eq!(asked(Some("twelve"), Some("many")), (0, DEFAULT_LIMIT));
	assert_eq!(asked(None, Some("0")), (0, 1));
	assert_eq!(asked(None, Some("9999")), (0, MAX_LIMIT));
}

#[tokio::test]
async fn a_node_is_set_read_and_deleted_with_every_change_kept() {
	let Some(router) = reached().await else { return };
	let (status, body) = ask(router.clone(), "/nodes").await;
	assert_eq!(status, StatusCode::OK);
	assert_eq!(body["data"], json!({ "nodes": {}, "change": 0 }));

	let (status, body) = put_node(router.clone(), "sin", "alice", sin()).await;
	assert_eq!(status, StatusCode::OK);
	let created = body["data"]["change"].as_i64().unwrap();
	let (status, body) = put_node(router.clone(), "sin", "alice", sin()).await;
	assert_eq!((status, &body["data"]), (StatusCode::OK, &json!({})));

	let (_, body) = ask(router.clone(), "/nodes").await;
	assert_eq!(body["data"], json!({ "nodes": { "sin": sin() }, "change": created }));

	let (status, body) = delete_node(router.clone(), "sin").await;
	assert_eq!(status, StatusCode::OK);
	let deleted = body["data"]["change"].as_i64().unwrap();
	assert!(deleted > created);
	let (status, body) = delete_node(router.clone(), "sin").await;
	assert_eq!((status, &body["data"]), (StatusCode::OK, &json!({})));

	let (status, body) = ask(router.clone(), &format!("/changes?after={created}")).await;
	assert_eq!(status, StatusCode::OK);
	let changes = body["data"]["changes"].as_array().unwrap();
	assert_eq!(changes.len(), 1);
	assert_eq!(changes[0]["id"], deleted);
	assert_eq!(changes[0]["before"], sin());
	assert!(changes[0].get("after").is_none());
	let at = changes[0]["at"].as_str().unwrap();
	assert!(at.parse::<jiff::Timestamp>().is_ok(), "{at}");

	let (_, body) = ask(router.clone(), "/changes?limit=1").await;
	let changes = body["data"]["changes"].as_array().unwrap();
	assert_eq!((changes.len(), &changes[0]["id"]), (1, &json!(created)));
	assert!(changes[0].get("before").is_none());
	assert_eq!(changes[0]["by"], "alice");
	assert_eq!(changes[0]["name"], "sin");

	assert_eq!(ask(router, "/health").await.0, StatusCode::OK);
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
