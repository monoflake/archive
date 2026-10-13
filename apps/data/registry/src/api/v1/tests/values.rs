//! An identity written with its value: kept and answered as the author wrote it, the catalog
//! marking it `masked`, and the secret service never asked, whether the registry is given it or
//! not -- RFC cpc2b.

use super::ids::ledger;
use super::{id_of, patch, put, wanted};
use crate::api::tests::{TOKEN, ask};
use crate::api::{Registry, routes};
use crate::secrets::{Secrets, fake};
use crate::store::testing::store;
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::Arc;

const VALUE: &str = "owner@example.com";

/// A registry over a store of its own, telling a fake ledger, given `secrets` or none.
async fn registry(secrets: Option<Secrets>) -> Option<Router> {
	let store = store().await?;
	let (base, _) = ledger().await;
	let ledger = Some(ledger::Ledger::to(base));
	Some(routes(Arc::new(Registry { store, token: TOKEN.into(), ledger, secrets })))
}

fn account(identities: Value) -> Value {
	json!({ "provider": "oracle", "identities": identities })
}

#[tokio::test]
async fn a_value_is_kept_and_answered_and_the_secret_service_asked_nothing() {
	let (base, secrets) = fake::serve().await;
	let Some(router) = registry(Some(Secrets::new(vec![base], "scoped".into()))).await else {
		return;
	};
	let identities = json!([
		{ "kind": "email", "value": VALUE },
		{ "kind": "tenancy", "value": "ocid1.tenancy.oc1..example" },
	]);
	let (status, answer) =
		put(router.clone(), "/v1/accounts/oci", wanted(0, account(identities.clone()))).await;
	assert_eq!(status, StatusCode::ACCEPTED, "{answer}");
	let (_, held) = ask(router.clone(), "/v1/accounts/oci").await;
	assert_eq!(held["data"]["spec"]["identities"], identities);
	assert!(secrets.lock().unwrap().asked.is_empty(), "{:?}", secrets.lock().unwrap().asked);

	let (id, at) = (id_of(&held["data"]), held["data"]["revision"].as_i64().unwrap());
	let one = json!([{ "kind": "user_id", "value": "ocid1.user.oc1..example" }]);
	let path = format!("/v1/accounts/{id}");
	let (status, _) = patch(router.clone(), &path, wanted(at, json!({ "identities": one }))).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let (_, held) = ask(router, "/v1/accounts/oci").await;
	assert_eq!(held["data"]["spec"]["identities"], one);
	assert!(secrets.lock().unwrap().asked.is_empty());
}

#[tokio::test]
async fn a_registry_without_the_secret_service_takes_a_value() {
	let Some(router) = registry(None).await else { return };
	let given = json!([{ "kind": "email", "value": VALUE }]);
	let (status, answer) = put(router, "/v1/accounts/oci", wanted(0, account(given))).await;
	assert_eq!(status, StatusCode::ACCEPTED, "{answer}");
}
