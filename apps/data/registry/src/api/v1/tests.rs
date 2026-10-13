use super::super::tests::{TOKEN, ask, code, reached, refusing, send, sin};
use axum::Router;
use axum::http::StatusCode;
use serde_json::{Value, json};

async fn put(router: Router, path: &str, body: Value) -> (StatusCode, Value) {
	let bearer = format!("Bearer {TOKEN}");
	send(router, "PUT", path, Some(&bearer), Some(body)).await
}

fn wanted(revision: i64, spec: Value) -> Value {
	json!({ "by": "test", "revision": revision, "spec": spec })
}

fn placed(placements: &[&str]) -> Value {
	json!({ "placements": placements })
}

#[tokio::test]
async fn writes_take_the_token_the_old_routes_take() {
	for path in ["/v1/nodes/sin", "/v1/apps/web"] {
		let body = wanted(0, json!({}));
		let closed = send(refusing(""), "PUT", path, Some("Bearer s3cret"), Some(body.clone())).await;
		assert_eq!(code(&closed), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
		let wrong = send(refusing(TOKEN), "PUT", path, Some("Bearer s3cre"), Some(body)).await;
		assert_eq!(code(&wrong), (StatusCode::UNAUTHORIZED, "invalid_token"));
	}
}

#[tokio::test]
async fn refuses_a_name_its_kind_cannot_have() {
	let router = refusing(TOKEN);
	for name in ["SIN", "sinn", "s1n"] {
		let answer = put(router.clone(), &format!("/v1/nodes/{name}"), wanted(0, sin())).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_name"), "{name}");
	}
	for name in ["-web", "Web", "we_b"] {
		let answer = put(router.clone(), &format!("/v1/apps/{name}"), wanted(0, placed(&[]))).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_name"), "{name}");
	}
}

#[tokio::test]
async fn status_and_drift_are_never_written() {
	let router = refusing(TOKEN);
	for (path, spec) in [("/v1/nodes/sin", sin()), ("/v1/apps/web", placed(&["tyo"]))] {
		for key in ["status", "drift"] {
			for value in [json!({}), json!(null)] {
				let mut body = wanted(0, spec.clone());
				body[key] = value;
				let answer = put(router.clone(), path, body).await;
				assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "forbidden_field"), "{path} {key}");
			}
		}
	}
	let imaged = wanted(0, json!({ "placements": [], "image": "x" }));
	let answer = put(router, "/v1/apps/web", imaged).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "forbidden_field"));
}

#[tokio::test]
async fn refuses_a_body_it_cannot_store() {
	let router = refusing(TOKEN);
	let mut tierless = sin();
	tierless.as_object_mut().unwrap().remove("tier");
	let nodes = [
		json!([]),
		json!({ "revision": 0, "spec": sin() }),
		json!({ "by": " ", "revision": 0, "spec": sin() }),
		json!({ "by": "test", "spec": sin() }),
		json!({ "by": "test", "revision": -1, "spec": sin() }),
		json!({ "by": "test", "revision": "3", "spec": sin() }),
		json!({ "by": "test", "revision": 1.5, "spec": sin() }),
		json!({ "by": "test", "revision": 0 }),
		wanted(0, tierless),
		wanted(0, json!({ "node": sin() })),
	];
	for body in nodes {
		let answer = put(router.clone(), "/v1/nodes/sin", body.clone()).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"), "{body}");
	}
	let apps = [
		wanted(0, json!({})),
		wanted(0, json!(["tyo"])),
		wanted(0, placed(&["workers"])),
		wanted(0, placed(&["tyo", "tyo"])),
	];
	for body in apps {
		let answer = put(router.clone(), "/v1/apps/web", body.clone()).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"), "{body}");
	}
}

#[tokio::test]
async fn the_catalog_is_answered_without_the_store() {
	let (status, body) = ask(refusing(TOKEN), "/v1/kinds").await;
	assert_eq!(status, StatusCode::OK);
	let items = body["data"]["items"].as_array().unwrap();
	let names: Vec<&str> = items.iter().map(|kind| kind["name"].as_str().unwrap()).collect();
	assert_eq!(names, ["node", "app"]);
	assert_eq!(body["data"]["items"][1]["fields"][0]["name"], "placements");
}

#[tokio::test]
async fn a_kind_it_does_not_hold_is_not_found() {
	for path in ["/v1/binding", "/v1/databases/main"] {
		let answer = ask(refusing(TOKEN), path).await;
		assert_eq!(code(&answer), (StatusCode::NOT_FOUND, "no_such_kind"), "{path}");
	}
}

#[tokio::test]
async fn an_app_is_set_and_read_with_its_revision() {
	let Some(router) = reached().await else { return };
	let (status, body) = ask(router.clone(), "/v1/apps").await;
	assert_eq!((status, &body["data"]), (StatusCode::OK, &json!({ "items": [], "revision": 0 })));
	let answer = ask(router.clone(), "/v1/apps/web").await;
	assert_eq!(code(&answer), (StatusCode::NOT_FOUND, "no_such_resource"));

	let (status, body) = put(router.clone(), "/v1/apps/web", wanted(0, placed(&["tyo"]))).await;
	assert_eq!(status, StatusCode::OK);
	let created = body["data"]["revision"].as_i64().unwrap();
	let (status, body) = ask(router.clone(), "/v1/apps/web").await;
	assert_eq!(status, StatusCode::OK);
	let web = json!({ "kind": "app", "name": "web", "revision": created, "spec": placed(&["tyo"]) });
	assert_eq!(body["data"], web, "status and drift are absent until a controller reports");

	let (status, body) = put(router.clone(), "/v1/apps/web", wanted(created, placed(&["tyo"]))).await;
	assert_eq!((status, &body["data"]), (StatusCode::OK, &json!({})));
	let answer = put(router.clone(), "/v1/apps/web", wanted(0, placed(&["sin"]))).await;
	assert_eq!(code(&answer), (StatusCode::CONFLICT, "invalid_revision"));
	let both = wanted(created, placed(&["sin", "tyo"]));
	let (_, body) = put(router.clone(), "/v1/apps/web", both).await;
	let moved = body["data"]["revision"].as_i64().unwrap();
	assert!(moved > created);
	let answer = put(router.clone(), "/v1/apps/web", wanted(created, placed(&["rdu"]))).await;
	assert_eq!(code(&answer), (StatusCode::CONFLICT, "invalid_revision"));

	let (_, body) = ask(router, "/v1/apps").await;
	let spec = placed(&["sin", "tyo"]);
	let web = json!({ "kind": "app", "name": "web", "revision": moved, "spec": spec });
	assert_eq!(body["data"], json!({ "items": [web], "revision": moved }));
}

#[tokio::test]
async fn apps_are_filtered_by_the_node_they_are_placed_on() {
	let Some(router) = reached().await else { return };
	let apps: [(&str, &[&str]); 3] = [("api", &["tyo", "sin"]), ("web", &["sin"]), ("cron", &[])];
	for (name, placements) in apps {
		put(router.clone(), &format!("/v1/apps/{name}"), wanted(0, placed(placements))).await;
	}
	let names = |body: &Value| -> Vec<String> {
		let items = body["data"]["items"].as_array().unwrap();
		items.iter().map(|app| app["name"].as_str().unwrap().to_owned()).collect()
	};
	let (_, every) = ask(router.clone(), "/v1/apps").await;
	assert_eq!(names(&every), ["api", "cron", "web"]);
	let (_, on_sin) = ask(router.clone(), "/v1/apps?node=sin").await;
	assert_eq!(names(&on_sin), ["api", "web"]);
	assert_eq!(on_sin["data"]["revision"], every["data"]["revision"]);
	let (_, on_tyo) = ask(router.clone(), "/v1/apps?node=tyo").await;
	assert_eq!(names(&on_tyo), ["api"]);
	let (status, on_rdu) = ask(router, "/v1/apps?node=rdu").await;
	assert_eq!((status, names(&on_rdu).len()), (StatusCode::OK, 0));
}

#[tokio::test]
async fn a_node_is_one_resource_through_both_routes() {
	let Some(router) = reached().await else { return };
	let answer = ask(router.clone(), "/v1/nodes/sin").await;
	assert_eq!(code(&answer), (StatusCode::NOT_FOUND, "no_such_node"));

	let (status, body) = put(router.clone(), "/v1/nodes/sin", wanted(0, sin())).await;
	assert_eq!(status, StatusCode::OK);
	let created = body["data"]["revision"].as_i64().unwrap();
	let (_, body) = ask(router.clone(), "/v1/nodes/sin").await;
	let node = json!({ "kind": "node", "name": "sin", "revision": created, "spec": sin() });
	assert_eq!(body["data"], node);
	let (_, body) = ask(router.clone(), "/v1/nodes").await;
	assert_eq!(body["data"], json!({ "items": [node], "revision": created }));

	// An app written moves neither what the old routes answer nor a node's own revision.
	put(router.clone(), "/v1/apps/web", wanted(0, placed(&["sin"]))).await;
	let (_, body) = ask(router.clone(), "/nodes").await;
	assert_eq!(body["data"], json!({ "nodes": { "sin": sin() }, "change": created }));
	let (_, body) = ask(router.clone(), "/changes").await;
	assert_eq!(body["data"]["changes"].as_array().unwrap().len(), 1);

	// The old route writes the same record, and a writer that read before it is refused.
	let mut moved = sin();
	moved["address"] = json!("100.64.0.10");
	let bearer = format!("Bearer {TOKEN}");
	let old = json!({ "by": "test", "node": moved });
	let (_, body) = send(router.clone(), "PUT", "/nodes/sin", Some(&bearer), Some(old)).await;
	let changed = body["data"]["change"].as_i64().unwrap();
	let answer = put(router.clone(), "/v1/nodes/sin", wanted(created, sin())).await;
	assert_eq!(code(&answer), (StatusCode::CONFLICT, "invalid_revision"));
	let (_, body) = ask(router, "/v1/nodes/sin").await;
	assert_eq!((&body["data"]["revision"], &body["data"]["spec"]), (&json!(changed), &moved));
}
