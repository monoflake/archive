//! Accounts, which have no name: each made by a `POST` that draws its id, and read and written by
//! that id alone -- RFC 32hd9.

use super::{id_of, patch, put, wanted};
use crate::api::tests::{TOKEN, ask, code, reached, refusing, send, sin};
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};

fn oracle() -> Value {
	json!({
		"provider": "oracle",
		"identities": [{ "kind": "email", "value": "me@example.com" }],
	})
}

async fn post(router: Router, path: &str, body: Value) -> (StatusCode, Value) {
	let bearer = format!("Bearer {TOKEN}");
	send(router, "POST", path, Some(&bearer), Some(body)).await
}

/// A new account's id, made by a `POST`.
async fn made(router: &Router, spec: Value) -> String {
	let (status, body) = post(router.clone(), "/v1/accounts", wanted(0, spec)).await;
	assert_eq!(status, StatusCode::ACCEPTED, "{body}");
	id_of(&body["data"])
}

#[tokio::test]
async fn an_account_is_made_by_post_and_read_and_written_by_its_id() {
	let Some(router) = reached().await else { return };
	let (status, body) = post(router.clone(), "/v1/accounts", wanted(0, oracle())).await;
	assert_eq!(status, StatusCode::ACCEPTED, "{body}");
	let id = id_of(&body["data"]);
	let created = body["data"]["revision"].as_i64().unwrap();
	assert!(crate::kinds::is_id(body["data"]["task"].as_str().unwrap()));

	let (status, held) = ask(router.clone(), &format!("/v1/accounts/{id}")).await;
	let resource =
		json!({ "kind": "account", "id": id, "name": null, "revision": created, "spec": oracle() });
	assert_eq!((status, &held["data"]), (StatusCode::OK, &resource));
	let (_, listed) = ask(router.clone(), "/v1/accounts").await;
	assert_eq!(listed["data"]["items"], json!([resource]));

	// A second with the same spec is a second account: nothing tells two apart but their ids.
	let other = made(&router, oracle()).await;
	assert_ne!(other, id);

	// A PATCH by id keeps what it does not name; a PUT by id sets the whole spec.
	let moved = wanted(created, json!({ "provider": "azure" }));
	let (status, body) = patch(router.clone(), &format!("/v1/accounts/{id}"), moved).await;
	assert_eq!(status, StatusCode::ACCEPTED, "{body}");
	assert!(body["data"].get("id").is_none(), "only a POST answers the id it made");
	let at = body["data"]["revision"].as_i64().unwrap();
	let mut spec = oracle();
	spec["provider"] = json!("azure");
	let (_, held) = ask(router.clone(), &format!("/v1/accounts/{id}")).await;
	assert_eq!(held["data"]["spec"], spec);
	let (status, _) = put(router.clone(), &format!("/v1/accounts/{id}"), wanted(at, oracle())).await;
	assert_eq!(status, StatusCode::ACCEPTED);
}

/// A name is no way to an account: not in a route, not in a body, not in a list's query.
#[tokio::test]
async fn an_account_takes_no_name_anywhere() {
	let router = refusing(TOKEN);
	for (method, path) in [("PUT", "/v1/accounts/oci"), ("PATCH", "/v1/accounts/oci")] {
		let bearer = format!("Bearer {TOKEN}");
		let answer = send(router.clone(), method, path, Some(&bearer), Some(wanted(0, oracle()))).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_name"), "{method}");
	}
	let answer = ask(router.clone(), "/v1/accounts?name=oci").await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_name"));
	let mut named = wanted(0, oracle());
	named["name"] = json!("oci");
	let answer = post(router.clone(), "/v1/accounts", named).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "forbidden_field"));

	let Some(router) = reached().await else { return };
	let answer = ask(router, "/v1/accounts/oci").await;
	assert_eq!(code(&answer), (StatusCode::NOT_FOUND, "no_such_resource"));
}

/// A named kind is made by its name, and a `POST` to it is answered with the route that does.
#[tokio::test]
async fn a_named_kind_takes_no_post() {
	for (plural, kind) in [("apps", "app"), ("nodes", "node")] {
		let answer = post(refusing(TOKEN), &format!("/v1/{plural}"), wanted(0, json!({}))).await;
		assert_eq!(code(&answer), (StatusCode::METHOD_NOT_ALLOWED, "invalid_method"), "{plural}");
		let message = answer.1["message"].as_str().unwrap();
		assert_eq!(message, format!("A {kind} is made by PUT /v1/{plural}/<name>"));
	}
}

/// An identity is exactly its kind and its value; a provider is one of those the registry knows.
#[tokio::test]
async fn an_account_is_refused_what_it_cannot_hold_before_the_store_is_asked() {
	let router = refusing(TOKEN);
	let named = json!({
		"provider": "oracle",
		"identities": [{ "kind": "email", "secret": "platform/registry/E", "value": "me@example.com" }],
	});
	let mut bare = named.clone();
	bare["identities"][0] = json!({ "kind": "email" });
	for (spec, expected) in [
		(named, "forbidden_field"),
		(bare, "invalid_body"),
		(json!({ "provider": "oracle", "name": "oci-2" }), "forbidden_field"),
		(json!({ "provider": "oci-2" }), "invalid_body"),
		(
			json!({ "provider": "cloud", "identities": [{ "kind": "email", "value": "x" }] }),
			"invalid_body",
		),
		(
			json!({ "provider": "oracle", "identities": [{ "kind": "password", "value": "x" }] }),
			"invalid_body",
		),
	] {
		let answer = post(router.clone(), "/v1/accounts", wanted(0, spec.clone())).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, expected), "{spec}");
	}
	let bearer = Some("Bearer s3cre");
	let answer = send(router, "POST", "/v1/accounts", bearer, Some(wanted(0, oracle()))).await;
	assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"));
}

/// An account is made only new: a POST at any revision but 0, and a write to an id not held, make
/// nothing.
#[tokio::test]
async fn a_write_to_an_account_not_held_makes_nothing() {
	let Some(router) = reached().await else { return };
	let answer = post(router.clone(), "/v1/accounts", wanted(3, oracle())).await;
	assert_eq!(code(&answer), (StatusCode::CONFLICT, "invalid_revision"));
	let path = "/v1/accounts/0123456789abcdef";
	let answer = put(router.clone(), path, wanted(0, oracle())).await;
	assert_eq!(code(&answer), (StatusCode::NOT_FOUND, "no_such_resource"));
	let (_, body) = ask(router, "/v1/accounts").await;
	assert_eq!(body["data"], json!({ "items": [], "revision": 0 }));
}

/// A node names the account it is under by its id, one the registry holds, and never a domain:
/// its failure domain is read from the account -- RFC 32hd9.
#[tokio::test]
async fn a_node_names_an_account_the_registry_holds_and_no_domain() {
	let Some(router) = reached().await else { return };
	let account = made(&router, oracle()).await;
	let (_, app) = put(router.clone(), "/v1/apps/web", wanted(0, json!({ "placements": [] }))).await;
	assert!(app["data"]["revision"].is_i64());
	let (_, web) = ask(router.clone(), "/v1/apps/web").await;
	let web = id_of(&web["data"]);

	let node = |key: &str, value: Value| {
		let mut spec = sin();
		spec[key] = value;
		spec
	};
	for (spec, expected) in [
		(node("account", json!("00000000000000aa")), "invalid_resource"),
		(node("account", json!(web)), "invalid_resource"),
		(node("account", json!("oci-2")), "invalid_body"),
		(node("domain", json!("oci-2")), "invalid_body"),
	] {
		let answer = put(router.clone(), "/v1/nodes/sin", wanted(0, spec.clone())).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, expected), "{spec}");
	}
	let (_, none) = ask(router.clone(), "/v1/nodes").await;
	assert_eq!(none["data"]["items"], json!([]), "a refused write makes nothing");

	let (status, body) =
		put(router.clone(), "/v1/nodes/sin", wanted(0, node("account", json!(account)))).await;
	assert_eq!(status, StatusCode::ACCEPTED, "{body}");
	let at = body["data"]["revision"].as_i64().unwrap();
	// A key given `null` is taken out, as a selfhost node's account is.
	let (status, _) =
		patch(router.clone(), "/v1/nodes/sin", wanted(at, json!({ "account": null }))).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let (_, sin) = ask(router.clone(), "/v1/nodes/sin").await;
	assert!(sin["data"]["spec"].get("account").is_none(), "{sin}");
	let at = sin["data"]["revision"].as_i64().unwrap();
	// A required key taken out is refused as a spec without it is.
	let answer = patch(router, "/v1/nodes/sin", wanted(at, json!({ "tier": null }))).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"));
}
