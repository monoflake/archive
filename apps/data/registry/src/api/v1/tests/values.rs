//! An identity written with its value: handed to the secret service, its name kept in its place,
//! and the value nowhere here -- not stored, not answered, not in the history or the ledger.

use super::ids::{ledger, taken};
use super::{id_of, patch, put, wanted};
use crate::api::tests::{TOKEN, ask, code};
use crate::api::{Registry, routes};
use crate::secrets::{Secrets, fake};
use crate::store::testing::{history_text, store};
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};

const VALUE: &str = "owner@example.com";
const SCOPED: &str = "scoped-token";

/// A registry over a store of its own, telling a fake ledger, handing values to `secrets`.
async fn registry(
	secrets: Option<Secrets>,
) -> Option<(Router, Arc<Registry>, Arc<Mutex<Vec<Value>>>)> {
	let store = store().await?;
	let (base, items) = ledger().await;
	let ledger = Some(ledger::Ledger::to(base));
	let registry = Arc::new(Registry { store, token: TOKEN.into(), ledger, secrets });
	Some((routes(registry.clone()), registry, items))
}

fn account(identities: Value) -> Value {
	json!({ "provider": "oracle", "identities": identities })
}

/// Nothing of `VALUE` in `text`, whatever carried it.
fn kept_nowhere(text: &str, place: &str) {
	assert!(!text.contains(VALUE), "the value is in {place}: {text}");
}

#[tokio::test]
async fn a_value_is_handed_to_the_secret_service_and_only_its_name_kept() {
	let (base, secrets) = fake::serve().await;
	let Some((router, registry, items)) =
		registry(Some(Secrets::new(vec![base], SCOPED.into()))).await
	else {
		return;
	};
	let identities = json!([
		{ "kind": "email", "value": VALUE },
		{ "kind": "tenancy", "secret": "platform/registry/OCI_TENANCY" },
	]);
	let (status, answer) =
		put(router.clone(), "/v1/accounts/oci", wanted(0, account(identities))).await;
	assert_eq!(status, StatusCode::ACCEPTED, "{answer}");
	kept_nowhere(&answer.to_string(), "the answer");

	let (_, held) = ask(router.clone(), "/v1/accounts/oci").await;
	let id = id_of(&held["data"]);
	let name = format!("ACCOUNT_{}_EMAIL", id.to_uppercase());
	let kept = json!([
		{ "kind": "email", "secret": format!("accounts/registry/{name}") },
		{ "kind": "tenancy", "secret": "platform/registry/OCI_TENANCY" },
	]);
	assert_eq!(held["data"]["spec"]["identities"], kept);
	kept_nowhere(&held.to_string(), "the account");

	// Under the account's own id, in the accounts scope, with the scope token, as a `secret`.
	let asked = secrets.lock().unwrap().asked.clone();
	assert_eq!(asked.len(), 2, "{asked:?}");
	let write = &asked[1];
	assert_eq!(
		(write.method, write.path.as_str()),
		("PUT", format!("/v1/secrets/registry/{name}").as_str())
	);
	assert_eq!(
		(write.scope.as_deref(), write.bearer.as_str()),
		(Some("accounts"), "Bearer scoped-token")
	);
	assert_eq!(write.body, json!({ "by": "test", "revision": 0, "value": VALUE, "class": "secret" }));

	kept_nowhere(&history_text(&registry.store).await, "the history");
	let told = taken(&items, 2).await;
	assert_eq!(told.len(), 2);
	kept_nowhere(&serde_json::to_string(&told).unwrap(), "the ledger");
	assert_eq!(told[0]["task"]["summary"]["id"], json!(id));
}

/// An account already held hands a value under the id it has, at the secret's own revision.
#[tokio::test]
async fn an_account_held_hands_a_value_under_the_id_it_has() {
	let (base, secrets) = fake::serve().await;
	let Some((router, _, _)) = registry(Some(Secrets::new(vec![base], SCOPED.into()))).await else {
		return;
	};
	let named = json!([{ "kind": "tenancy", "secret": "platform/registry/OCI_TENANCY" }]);
	put(router.clone(), "/v1/accounts/oci", wanted(0, account(named))).await;
	assert!(secrets.lock().unwrap().asked.is_empty(), "a name alone asks the secret service nothing");
	let (_, held) = ask(router.clone(), "/v1/accounts/oci").await;
	let (id, at) = (id_of(&held["data"]), held["data"]["revision"].as_i64().unwrap());

	let name = format!("ACCOUNT_{}_USER_ID", id.to_uppercase());
	secrets.lock().unwrap().revisions.insert(name.clone(), 4);
	let both = json!([
		{ "kind": "tenancy", "secret": "platform/registry/OCI_TENANCY" },
		{ "kind": "user_id", "value": VALUE },
	]);
	let (status, _) =
		patch(router.clone(), &format!("/v1/accounts/{id}"), wanted(at, json!({ "identities": both })))
			.await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let written = secrets.lock().unwrap().asked.last().cloned().unwrap();
	assert_eq!(
		(written.path, written.body["revision"].clone()),
		(format!("/v1/secrets/registry/{name}"), json!(4))
	);
	let (_, held) = ask(router, "/v1/accounts/oci").await;
	assert_eq!(
		held["data"]["spec"]["identities"][1]["secret"],
		json!(format!("accounts/registry/{name}"))
	);
}

/// A value the secret service did not take changes nothing here, and is answered without it.
#[tokio::test]
async fn a_value_the_secret_service_did_not_take_changes_nothing() {
	let (base, secrets) = fake::serve().await;
	secrets.lock().unwrap().refusing = Some((401, "invalid_token"));
	let unreachable = fake::unreachable().await;
	for (cores, said) in [(vec![base], "invalid_token"), (vec![unreachable], "no core")] {
		let Some((router, registry, items)) = registry(Some(Secrets::new(cores, SCOPED.into()))).await
		else {
			return;
		};
		let given = json!([{ "kind": "email", "value": VALUE }]);
		let answer = put(router.clone(), "/v1/accounts/oci", wanted(0, account(given))).await;
		assert_eq!(code(&answer), (StatusCode::BAD_GATEWAY, "upstream_unavailable"), "{said}");
		let message = answer.1["message"].as_str().unwrap();
		assert!(message.contains(said), "{message}");
		kept_nowhere(message, "the refusal");
		let absent = ask(router, "/v1/accounts/oci").await;
		assert_eq!(code(&absent), (StatusCode::NOT_FOUND, "no_such_resource"));
		assert!(history_text(&registry.store).await.is_empty(), "nothing is written");
		tokio::time::sleep(std::time::Duration::from_millis(300)).await;
		assert!(items.lock().unwrap().is_empty(), "no task is told");
	}
}

/// A registry not given the secret service refuses a value, and still takes a name.
#[tokio::test]
async fn without_the_secret_service_a_value_is_refused_and_a_name_taken() {
	let Some((router, _, _)) = registry(None).await else { return };
	let given = json!([{ "kind": "email", "value": VALUE }]);
	let answer = put(router.clone(), "/v1/accounts/oci", wanted(0, account(given))).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
	kept_nowhere(&answer.1.to_string(), "the refusal");
	let named = json!([{ "kind": "email", "secret": "platform/registry/OCI_EMAIL" }]);
	let (status, _) = put(router, "/v1/accounts/oci", wanted(0, account(named))).await;
	assert_eq!(status, StatusCode::ACCEPTED);
}
