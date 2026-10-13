//! Accounts and domains, resources as nodes and apps are -- RFC 32hd9.

use super::{id_of, patch, put, wanted};
use crate::api::tests::{TOKEN, ask, code, reached, refusing, send, sin};
use axum::http::StatusCode;
use serde_json::{Value, json};

fn oracle() -> Value {
	json!({
		"provider": "oracle",
		"identities": [{ "kind": "email", "value": "me@example.com" }],
	})
}

#[tokio::test]
async fn an_account_and_a_domain_are_set_and_read_by_name_and_by_id() {
	let Some(router) = reached().await else { return };
	for (kind, plural, spec) in [
		("account", "accounts", oracle()),
		("domain", "domains", json!({ "note": "Oracle's Tokyo and Singapore" })),
	] {
		let answer = ask(router.clone(), &format!("/v1/{plural}/oci-2")).await;
		assert_eq!(code(&answer), (StatusCode::NOT_FOUND, "no_such_resource"), "{kind}");
		let (status, body) =
			put(router.clone(), &format!("/v1/{plural}/oci-2"), wanted(0, spec.clone())).await;
		assert_eq!(status, StatusCode::ACCEPTED, "{kind}: {body}");
		let created = body["data"]["revision"].as_i64().unwrap();
		let (_, by_name) = ask(router.clone(), &format!("/v1/{plural}/oci-2")).await;
		let id = id_of(&by_name["data"]);
		let resource =
			json!({ "kind": kind, "id": id, "name": "oci-2", "revision": created, "spec": spec });
		assert_eq!(by_name["data"], resource);
		let (_, by_id) = ask(router.clone(), &format!("/v1/{plural}/{id}")).await;
		assert_eq!(by_id["data"], resource);
		let (_, listed) = ask(router.clone(), &format!("/v1/{plural}?name=oci-2")).await;
		assert_eq!(listed["data"]["items"], json!([resource]));
		let (_, none) = ask(router.clone(), &format!("/v1/{plural}?name=oci")).await;
		assert_eq!(none["data"]["items"], json!([]));
	}

	// A PATCH by id keeps what it does not name.
	let (_, account) = ask(router.clone(), "/v1/accounts/oci-2").await;
	let (id, at) = (id_of(&account["data"]), account["data"]["revision"].as_i64().unwrap());
	let renamed = wanted(at, json!({ "provider": "oraclecloud" }));
	let (status, _) = patch(router.clone(), &format!("/v1/accounts/{id}"), renamed).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let (_, account) = ask(router, "/v1/accounts/oci-2").await;
	let mut spec = oracle();
	spec["provider"] = json!("oraclecloud");
	assert_eq!(account["data"]["spec"], spec);
}

/// An identity is exactly its kind and its value.
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
			json!({ "provider": "oracle", "identities": [{ "kind": "password", "value": "x" }] }),
			"invalid_body",
		),
	] {
		let answer = put(router.clone(), "/v1/accounts/oci-2", wanted(0, spec.clone())).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, expected), "{spec}");
	}
	let answer = put(router.clone(), "/v1/domains/oci-2", wanted(0, json!({ "note": "" }))).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"));
	for path in ["/v1/accounts/OCI-2", "/v1/domains/-rdu", "/v1/domains/oci_2"] {
		let answer = put(router.clone(), path, wanted(0, json!({ "note": "x" }))).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_name"), "{path}");
	}
	let bearer = Some("Bearer s3cre");
	let answer = send(router, "PUT", "/v1/domains/rdu", bearer, Some(wanted(0, json!({})))).await;
	assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"));
}

/// A resource is made by its name alone, an account and a domain as every other.
#[tokio::test]
async fn a_write_to_an_account_or_domain_id_not_held_makes_nothing() {
	let Some(router) = reached().await else { return };
	for path in ["/v1/accounts/0123456789abcdef", "/v1/domains/0123456789abcdef"] {
		let answer = put(router.clone(), path, wanted(0, json!({ "provider": "oracle" }))).await;
		assert_eq!(code(&answer), (StatusCode::NOT_FOUND, "no_such_resource"), "{path}");
	}
	let (_, body) = ask(router, "/v1/accounts").await;
	assert_eq!(body["data"], json!({ "items": [], "revision": 0 }));
}

/// What a node names by shape: an id has to be one the registry holds of that kind, and a code is
/// still taken until the import writes ids.
#[tokio::test]
async fn a_node_names_an_account_and_a_domain_the_registry_holds() {
	let Some(router) = reached().await else { return };
	put(router.clone(), "/v1/accounts/oci-2", wanted(0, oracle())).await;
	put(router.clone(), "/v1/domains/oci-2", wanted(0, json!({}))).await;
	let (_, account) = ask(router.clone(), "/v1/accounts/oci-2").await;
	let (_, domain) = ask(router.clone(), "/v1/domains/oci-2").await;
	let (account, domain) = (id_of(&account["data"]), id_of(&domain["data"]));

	let node = |account: &str, domain: &str| {
		let mut spec = sin();
		spec["account"] = json!(account);
		spec["domain"] = json!(domain);
		spec
	};
	// Each refused naming its field: an id held by nobody, and an id held by the other kind.
	for (spec, field) in [
		(node("00000000000000aa", &domain), "account"),
		(node(&account, "00000000000000bb"), "domain"),
		(node(&domain, &domain), "account"),
		(node(&account, &account), "domain"),
	] {
		let answer = put(router.clone(), "/v1/nodes/sin", wanted(0, spec)).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_resource"), "{field}");
		assert!(answer.1["message"].as_str().unwrap().starts_with(&format!("`{field}`")), "{answer:?}");
	}
	let (_, none) = ask(router.clone(), "/v1/nodes").await;
	assert_eq!(none["data"]["items"], json!([]), "a refused write makes nothing");

	// The old codes, as every node holds them until the import writes ids.
	let (status, body) =
		put(router.clone(), "/v1/nodes/sin", wanted(0, node("oci-2", "oci-2"))).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let at = body["data"]["revision"].as_i64().unwrap();
	let (status, body) =
		put(router.clone(), "/v1/nodes/sin", wanted(at, node(&account, &domain))).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let at = body["data"]["revision"].as_i64().unwrap();
	let (_, sin) = ask(router.clone(), "/v1/nodes/sin").await;
	assert_eq!(
		(&sin["data"]["spec"]["account"], &sin["data"]["spec"]["domain"]),
		(&json!(account), &json!(domain))
	);

	// A key given `null` is taken out, as a selfhost node's account is.
	let (status, _) =
		patch(router.clone(), "/v1/nodes/sin", wanted(at, json!({ "account": null }))).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let (_, sin) = ask(router.clone(), "/v1/nodes/sin").await;
	assert!(sin["data"]["spec"].get("account").is_none(), "{sin}");
	let at = sin["data"]["revision"].as_i64().unwrap();
	// A required key taken out is refused as a spec without it is.
	let answer = patch(router, "/v1/nodes/sin", wanted(at, json!({ "domain": null }))).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"));
}
