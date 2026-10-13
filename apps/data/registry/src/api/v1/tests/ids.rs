//! Ids, `?name=`, and a write as a task told to the ledger -- RFC 2kuvi and RFC cpc2b.

use super::{id_of, patch, placed, put, release, wanted};
use crate::api::tests::{TOKEN, ask, code, reached, sin};
use crate::api::{Registry, routes};
use crate::store::testing::store;
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const ABSENT: &str = "0123456789abcdef";

#[tokio::test]
async fn a_resource_is_read_and_written_by_its_id_as_by_its_name() {
	let Some(router) = reached().await else { return };
	let (status, body) = put(router.clone(), "/v1/apps/web", wanted(0, placed(&["tyo"]))).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let created = body["data"]["revision"].as_i64().unwrap();
	let (_, by_name) = ask(router.clone(), "/v1/apps/web").await;
	let id = id_of(&by_name["data"]);
	let (status, by_id) = ask(router.clone(), &format!("/v1/apps/{id}")).await;
	assert_eq!((status, &by_id["data"]), (StatusCode::OK, &by_name["data"]));

	let both = wanted(created, placed(&["tyo", "sin"]));
	let (status, body) = put(router.clone(), &format!("/v1/apps/{id}"), both).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let moved = body["data"]["revision"].as_i64().unwrap();
	let released = wanted(moved, json!({ "release": release(7) }));
	let (status, _) = patch(router.clone(), &format!("/v1/apps/{id}"), released).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let (_, body) = ask(router.clone(), "/v1/apps/web").await;
	assert_eq!(body["data"]["id"], json!(id), "an id is never changed");
	assert_eq!(body["data"]["spec"], json!({ "placements": ["tyo", "sin"], "release": release(7) }));

	let (_, body) = put(router.clone(), "/v1/nodes/sin", wanted(0, sin())).await;
	let node_at = body["data"]["revision"].as_i64().unwrap();
	let (_, node) = ask(router.clone(), "/v1/nodes/sin").await;
	let node_id = id_of(&node["data"]);
	assert_ne!(node_id, id);
	let retiring = wanted(node_at, json!({ "retired": true }));
	let (status, _) = patch(router.clone(), &format!("/v1/nodes/{node_id}"), retiring).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let (_, node) = ask(router.clone(), &format!("/v1/nodes/{node_id}")).await;
	assert_eq!(
		(&node["data"]["name"], &node["data"]["spec"]["retired"]),
		(&json!("sin"), &json!(true))
	);
	// An app's id names no node.
	let answer = ask(router, &format!("/v1/nodes/{id}")).await;
	assert_eq!(code(&answer), (StatusCode::NOT_FOUND, "no_such_node"));
}

/// A resource is made by its name alone: a key of an id's shape is an id, never a new name.
#[tokio::test]
async fn a_write_to_an_id_not_held_is_not_found_and_makes_nothing() {
	let Some(router) = reached().await else { return };
	for (path, spec, absent) in [
		(format!("/v1/nodes/{ABSENT}"), sin(), "no_such_node"),
		(format!("/v1/apps/{ABSENT}"), placed(&["tyo"]), "no_such_resource"),
	] {
		for method in ["PUT", "PATCH"] {
			let answer = if method == "PUT" {
				put(router.clone(), &path, wanted(0, spec.clone())).await
			} else {
				patch(router.clone(), &path, wanted(0, spec.clone())).await
			};
			assert_eq!(code(&answer), (StatusCode::NOT_FOUND, absent), "{method} {path}");
		}
		assert_eq!(code(&ask(router.clone(), &path).await), (StatusCode::NOT_FOUND, absent));
	}
	for list in ["/v1/nodes", "/v1/apps"] {
		let (_, body) = ask(router.clone(), list).await;
		assert_eq!(body["data"], json!({ "items": [], "revision": 0 }), "{list}");
	}
}

#[tokio::test]
async fn a_list_finds_one_by_name() {
	let Some(router) = reached().await else { return };
	for name in ["api", "web"] {
		put(router.clone(), &format!("/v1/apps/{name}"), wanted(0, placed(&["tyo"]))).await;
	}
	put(router.clone(), "/v1/nodes/sin", wanted(0, sin())).await;
	let names = |body: &Value| -> Vec<String> {
		let items = body["data"]["items"].as_array().unwrap();
		items.iter().map(|item| item["name"].as_str().unwrap().to_owned()).collect()
	};
	let (_, every) = ask(router.clone(), "/v1/apps").await;
	let (_, web) = ask(router.clone(), "/v1/apps?name=web").await;
	assert_eq!(names(&web), ["web"]);
	assert_eq!(web["data"]["revision"], every["data"]["revision"]);
	let (_, both) = ask(router.clone(), "/v1/apps?name=web&node=tyo").await;
	assert_eq!(names(&both), ["web"]);
	let (_, neither) = ask(router.clone(), "/v1/apps?name=web&node=rdu").await;
	assert!(names(&neither).is_empty());
	let (_, sin) = ask(router.clone(), "/v1/nodes?name=sin").await;
	assert_eq!(names(&sin), ["sin"]);
	let (status, none) = ask(router, "/v1/nodes?name=tyo").await;
	assert_eq!((status, names(&none).len()), (StatusCode::OK, 0));
}

/// A ledger that takes `POST /events` and keeps every item it is sent, in order.
pub(super) async fn ledger() -> (String, Arc<Mutex<Vec<Value>>>) {
	let taken: Arc<Mutex<Vec<Value>>> = Arc::default();
	let kept = taken.clone();
	let events = axum::routing::post(move |axum::Json(batch): axum::Json<Vec<Value>>| {
		let kept = kept.clone();
		async move {
			kept.lock().unwrap().extend(batch);
			axum::http::StatusCode::OK
		}
	});
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	tokio::spawn(async move { axum::serve(listener, Router::new().route("/events", events)).await });
	(format!("http://{address}"), taken)
}

/// The items the ledger took once it has `count`, within five seconds.
pub(super) async fn taken(items: &Mutex<Vec<Value>>, count: usize) -> Vec<Value> {
	for _ in 0..50 {
		if items.lock().unwrap().len() >= count {
			break;
		}
		tokio::time::sleep(Duration::from_millis(100)).await;
	}
	items.lock().unwrap().clone()
}

/// What changes is a task the ledger is told, done, with one event; what changes nothing is none.
#[tokio::test]
async fn a_write_that_changes_something_is_a_task_told_to_the_ledger() {
	let Some(store) = store().await else { return };
	let (base, items) = ledger().await;
	let ledger = Some(ledger::Ledger::to(base));
	let router = routes(Arc::new(Registry { store, token: TOKEN.into(), ledger, secrets: None }));

	let (status, body) = put(router.clone(), "/v1/apps/web", wanted(0, placed(&["tyo"]))).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let created = body["data"]["revision"].as_i64().unwrap();
	let first = body["data"]["task"].as_str().unwrap().to_owned();
	assert!(crate::kinds::is_id(&first), "{body}");
	assert_eq!(body["data"].as_object().unwrap().len(), 2, "{body}");
	let (_, app) = ask(router.clone(), "/v1/apps/web").await;
	let id = id_of(&app["data"]);

	let (status, body) = put(router.clone(), "/v1/apps/web", wanted(created, placed(&["tyo"]))).await;
	assert_eq!((status, &body["data"]), (StatusCode::OK, &json!({})));
	let (status, body) =
		patch(router.clone(), "/v1/apps/web", wanted(created, json!({ "release": release(7) }))).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	let released = body["data"]["revision"].as_i64().unwrap();
	let second = body["data"]["task"].as_str().unwrap().to_owned();
	assert_ne!(first, second);

	// In the order handed over, so a task for the write that changed nothing would sit between.
	let items = taken(&items, 4).await;
	assert_eq!(items.len(), 4, "{items:?}");
	let task = &items[0]["task"];
	assert_eq!(task["service"], "registry");
	assert_eq!(task["id"], json!(first));
	let what = (&task["kind"], &task["state"], &task["caller"]);
	assert_eq!(what, (&json!("write"), &json!("done"), &json!("ours")));
	assert_eq!(task["asked_at"], task["finished_at"]);
	assert_eq!(
		task["summary"],
		json!({ "kind": "app", "id": id, "name": "web", "revision": created })
	);
	let event = &items[1]["event"];
	assert_eq!((&event["service"], &event["task"]), (&json!("registry"), &json!(first)));
	assert_eq!((&event["stage"], &event["level"]), (&json!("written"), &json!("info")));
	assert_eq!(event["message"], format!("test wrote app web at revision {created}"));
	assert_eq!(items[2]["task"]["id"], json!(second));
	assert_eq!(items[2]["task"]["summary"]["revision"], json!(released));
	assert_eq!(items[3]["event"]["task"], json!(second));
}

/// With no ledger, a write is answered the same, and told nowhere.
#[tokio::test]
async fn a_write_without_a_ledger_is_answered_the_same() {
	let Some(router) = reached().await else { return };
	let (status, body) = put(router, "/v1/nodes/sin", wanted(0, sin())).await;
	assert_eq!(status, StatusCode::ACCEPTED);
	assert!(crate::kinds::is_id(body["data"]["task"].as_str().unwrap_or_default()), "{body}");
}
