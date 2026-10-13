use super::*;
use crate::seal::{self, Share};
use crate::store::testing::{store, unreached};
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

pub(crate) const TOKEN: &str = "write-s3cret";
pub(crate) const PEER: &str = "peer-s3cret";
const TYO: &str = "tyo-read-s3cret";
const SIN: &str = "sin-read-s3cret";

pub(crate) fn node_tokens() -> NodeTokens {
	let value =
		format!("tyo:{} sin:{}", hex::encode(tokens::digest(TYO)), hex::encode(tokens::digest(SIN)));
	NodeTokens::parse(&value).unwrap()
}

pub(crate) fn secrets(store: Store, seal: Arc<Seal>, registries: Vec<String>) -> Shared {
	Arc::new(Secrets {
		store,
		seal,
		token: TOKEN.into(),
		peer_token: PEER.into(),
		nodes: node_tokens(),
		registries,
		client: peers::client(),
	})
}

/// A router over a database never reached, sealed, for what is refused before the store is asked.
fn refusing() -> (Router, Arc<Seal>) {
	let seal = Arc::new(Seal::default());
	(routes(secrets(unreached(), seal.clone(), Vec::new())), seal)
}

/// A base address on loopback where nothing listens: port 1, which no test binds.
pub(crate) fn nobody() -> String {
	let host = "127.0.0.1";
	format!("http://{host}:1")
}

/// The base address of `router`, served on a port of its own on loopback until the test ends.
pub(crate) async fn serve(router: Router) -> String {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
	format!("http://{address}")
}

/// A registry that places `gemini` and `shot` on `tyo`, and `grok` on `sin`, answering as the
/// registry's `GET /v1/apps?node=` does.
async fn fake_registry() -> String {
	#[derive(serde::Deserialize)]
	struct Asked {
		node: Option<String>,
	}
	let placed = [("gemini", ["tyo"].as_slice()), ("grok", &["sin"]), ("shot", &["tyo", "sin"])];
	let router = Router::new().route(
		"/v1/apps",
		get(move |Query(asked): Query<Asked>| async move {
			let items: Vec<Value> = placed
				.iter()
				.filter(|(_, nodes)| asked.node.as_deref().is_none_or(|node| nodes.contains(&node)))
				.map(|(name, nodes)| {
					json!({ "kind": "app", "name": name, "revision": 1, "spec": { "placements": nodes } })
				})
				.collect();
			response::success(StatusCode::OK, json!({ "items": items, "revision": 9 }))
		}),
	);
	serve(router).await
}

pub(crate) async fn send(
	router: Router,
	method: &str,
	path: &str,
	bearer: Option<&str>,
	body: Option<Value>,
) -> (StatusCode, Value) {
	let mut request = Request::builder().method(method).uri(path);
	if let Some(bearer) = bearer {
		request = request.header("authorization", format!("Bearer {bearer}"));
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

pub(crate) fn code(answer: &(StatusCode, Value)) -> (StatusCode, &str) {
	(answer.0, answer.1["code"].as_str().unwrap_or_default())
}

fn wanted(revision: i64, value: &str) -> Value {
	json!({ "by": "test", "revision": revision, "value": value })
}

#[test]
fn every_code_answered_is_in_the_catalog() {
	let named = response::codes_named(include_str!("../api.rs"));
	assert!(named.len() > 5, "{named:?}");
	for code in named {
		assert!(response::message_of(code).is_some(), "{code} is not in the response catalog");
	}
}

#[tokio::test]
async fn every_route_that_touches_a_secret_is_unavailable_while_sealed() {
	let (router, _) = refusing();
	let answers = [
		send(router.clone(), "GET", "/v1/secrets", Some(TOKEN), None).await,
		send(router.clone(), "PUT", "/v1/secrets/gemini/KEY", Some(TOKEN), Some(wanted(0, "v"))).await,
		send(router.clone(), "GET", "/v1/nodes/tyo/secrets", Some(TYO), None).await,
	];
	for answer in answers {
		assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
		assert!(answer.1["message"].as_str().unwrap().starts_with("Sealed"), "{}", answer.1);
	}
	let state = send(router, "GET", "/v1/seal", None, None).await;
	assert_eq!(state.0, StatusCode::OK);
	assert_eq!(state.1["data"], json!({ "sealed": true, "share": false }));
}

#[tokio::test]
async fn writes_take_the_write_token() {
	let closed = Secrets {
		token: String::new(),
		..Arc::into_inner(secrets(unreached(), Arc::default(), Vec::new())).unwrap()
	};
	let closed = routes(Arc::new(closed));
	let answer =
		send(closed.clone(), "PUT", "/v1/secrets/gemini/KEY", Some(TOKEN), Some(wanted(0, "v"))).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
	let answer = send(closed, "GET", "/v1/secrets", Some(TOKEN), None).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
	let (router, _) = refusing();
	for bearer in [None, Some(""), Some("write-s3cre"), Some(PEER), Some(TYO)] {
		let answer =
			send(router.clone(), "PUT", "/v1/secrets/gemini/KEY", bearer, Some(wanted(0, "v"))).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
		let answer =
			send(router.clone(), "POST", "/v1/seal", bearer, Some(json!({ "key": "00" }))).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
		let answer = send(router.clone(), "GET", "/v1/secrets", bearer, None).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
	}
}

#[tokio::test]
async fn a_node_reads_with_its_own_token_alone() {
	let (router, _) = refusing();
	for (path, bearer) in [
		("/v1/nodes/tyo/secrets", Some(SIN)),
		("/v1/nodes/tyo/secrets", Some(TOKEN)),
		("/v1/nodes/tyo/secrets", None),
		("/v1/nodes/sin/secrets", Some(TYO)),
		("/v1/nodes/rdu/secrets", Some(TYO)),
		("/v1/nodes/rdu/secrets", Some("")),
	] {
		let answer = send(router.clone(), "GET", path, bearer, None).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{path} {bearer:?}");
	}
	for node in ["TYO", "tyoo", "t1o"] {
		let answer =
			send(router.clone(), "GET", &format!("/v1/nodes/{node}/secrets"), Some(TYO), None).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_name"), "{node}");
	}
}

#[tokio::test]
async fn a_share_is_given_to_the_peer_token_alone_and_only_while_held() {
	let (router, seal) = refusing();
	for bearer in [None, Some(TOKEN), Some(TYO), Some("peer-s3cre")] {
		let answer = send(router.clone(), "GET", "/v1/seal/share", bearer, None).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
	}
	let none = send(router.clone(), "GET", "/v1/seal/share", Some(PEER), None).await;
	assert_eq!(code(&none), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));

	let share = seal::split(&RootKey::generate()).remove(1);
	seal.hold(share.clone());
	let given = send(router.clone(), "GET", "/v1/seal/share", Some(PEER), None).await;
	assert_eq!(given.0, StatusCode::OK);
	let written: seal::Written = serde_json::from_value(given.1["data"].clone()).unwrap();
	assert!(Share::from_written(&written).unwrap() == share);
	assert!(seal.is_sealed(), "a share given opens nothing");

	let unset = Secrets {
		peer_token: String::new(),
		..Arc::into_inner(secrets(unreached(), seal, Vec::new())).unwrap()
	};
	let answer = send(routes(Arc::new(unset)), "GET", "/v1/seal/share", Some(""), None).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
}

#[tokio::test]
async fn the_operator_opens_a_lone_core_with_the_whole_key() {
	let (router, seal) = refusing();
	let key = RootKey::generate();
	seal.hold(seal::split(&key).remove(0));
	for body in [json!({}), json!({ "key": "abcd" }), json!({ "key": 5 }), json!([])] {
		let answer = send(router.clone(), "POST", "/v1/seal", Some(TOKEN), Some(body.clone())).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"), "{body}");
	}
	let other = json!({ "key": *RootKey::generate().to_hex() });
	let answer = send(router.clone(), "POST", "/v1/seal", Some(TOKEN), Some(other.clone())).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"));
	assert!(seal.is_sealed());

	let right = json!({ "key": *key.to_hex() });
	let answer = send(router.clone(), "POST", "/v1/seal", Some(TOKEN), Some(right.clone())).await;
	assert_eq!(answer.0, StatusCode::OK);
	assert_eq!(answer.1["data"], json!({ "sealed": false }));
	assert_eq!(seal.with_key(|held| *held.bytes()), Some(*key.bytes()));

	let again = send(router.clone(), "POST", "/v1/seal", Some(TOKEN), Some(right)).await;
	assert_eq!(again.0, StatusCode::OK);
	let answer = send(router, "POST", "/v1/seal", Some(TOKEN), Some(other)).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"));
	assert_eq!(seal.with_key(|held| *held.bytes()), Some(*key.bytes()));
}

#[tokio::test]
async fn refuses_a_secret_it_cannot_store() {
	let seal = Arc::new(Seal::default());
	seal.open(RootKey::generate());
	let router = routes(secrets(unreached(), seal, Vec::new()));
	let put =
		|path: &'static str, body: Value| send(router.clone(), "PUT", path, Some(TOKEN), Some(body));
	for path in
		["/v1/secrets/Gemini/KEY", "/v1/secrets/-gemini/KEY", "/v1/secrets/gemini/KEY?scope=Platform"]
	{
		assert_eq!(
			code(&put(path, wanted(0, "v")).await),
			(StatusCode::BAD_REQUEST, "invalid_name"),
			"{path}"
		);
	}
	for path in ["/v1/secrets/gemini/key", "/v1/secrets/gemini/1KEY", "/v1/secrets/gemini/K-EY"] {
		assert_eq!(
			code(&put(path, wanted(0, "v")).await),
			(StatusCode::BAD_REQUEST, "invalid_variable"),
			"{path}"
		);
	}
	for value in ["two\nlines", "carriage\rreturn", "nul\0"] {
		let answer = put("/v1/secrets/gemini/KEY", wanted(0, value)).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_variable"), "{value:?}");
	}
	let large = "x".repeat(MAX_VALUE_BYTES + 1);
	assert_eq!(
		code(&put("/v1/secrets/gemini/KEY", wanted(0, &large)).await),
		(StatusCode::BAD_REQUEST, "invalid_size")
	);
	for body in [
		json!([]),
		json!({ "revision": 0, "value": "v" }),
		json!({ "by": " ", "revision": 0, "value": "v" }),
		json!({ "by": "test", "value": "v" }),
		json!({ "by": "test", "revision": -1, "value": "v" }),
		json!({ "by": "test", "revision": "1", "value": "v" }),
		json!({ "by": "test", "revision": 0 }),
		json!({ "by": "test", "revision": 0, "value": 5 }),
	] {
		let answer = put("/v1/secrets/gemini/KEY", body.clone()).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"), "{body}");
	}
	let mut extra = wanted(0, "v");
	extra["spec"] = json!({});
	assert_eq!(
		code(&put("/v1/secrets/gemini/KEY", extra).await),
		(StatusCode::BAD_REQUEST, "forbidden_field")
	);
}

#[tokio::test]
async fn health_says_sealed_and_fails_only_on_the_store() {
	let (router, _) = refusing();
	let answer = send(router, "GET", "/health", None, None).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "store_unavailable"));
	let Some(store) = store().await else { return };
	let router = routes(secrets(store, Arc::default(), Vec::new()));
	let answer = send(router, "GET", "/health", None, None).await;
	assert_eq!(answer.0, StatusCode::OK);
	assert_eq!(answer.1["data"], json!({ "sealed": true }));
}

#[tokio::test]
async fn a_secret_written_is_listed_without_its_value_and_read_by_its_node() {
	let Some(store) = store().await else { return };
	let seal = Arc::new(Seal::default());
	seal.open(RootKey::generate());
	let unreachable = nobody();
	let router = routes(secrets(store, seal, vec![unreachable, fake_registry().await]));
	let put =
		|path: &'static str, body: Value| send(router.clone(), "PUT", path, Some(TOKEN), Some(body));

	let first = put("/v1/secrets/gemini/GEMINI_KEY", wanted(0, "gemini-s3cret")).await;
	assert_eq!(first.0, StatusCode::OK, "{}", first.1);
	let revision = first.1["data"]["revision"].as_i64().unwrap();
	let stale = put("/v1/secrets/gemini/GEMINI_KEY", wanted(0, "other")).await;
	assert_eq!(code(&stale), (StatusCode::CONFLICT, "invalid_revision"));
	let same = put("/v1/secrets/gemini/GEMINI_KEY", wanted(revision, "gemini-s3cret")).await;
	assert_eq!((same.0, &same.1["data"]), (StatusCode::OK, &json!({})));
	put("/v1/secrets/grok/GROK_KEY", wanted(0, "grok-s3cret")).await;
	put("/v1/secrets/shot/SHOT_KEY?scope=canmi", wanted(0, "shot-s3cret")).await;

	let listed = send(router.clone(), "GET", "/v1/secrets", Some(TOKEN), None).await;
	assert_eq!(listed.0, StatusCode::OK);
	let text = listed.1.to_string();
	assert!(!text.contains("s3cret"), "{text}");
	let items = listed.1["data"]["items"].as_array().unwrap();
	assert_eq!(items.len(), 3);
	assert_eq!(
		items[0],
		json!({ "scope": "canmi", "app": "shot", "name": "SHOT_KEY", "revision": items[0]["revision"] })
	);

	let tyo = send(router.clone(), "GET", "/v1/nodes/tyo/secrets", Some(TYO), None).await;
	assert_eq!(tyo.0, StatusCode::OK, "{}", tyo.1);
	let values: Vec<(&str, &str)> = tyo.1["data"]["items"]
		.as_array()
		.unwrap()
		.iter()
		.map(|item| (item["name"].as_str().unwrap(), item["value"].as_str().unwrap()))
		.collect();
	assert_eq!(values, [("SHOT_KEY", "shot-s3cret"), ("GEMINI_KEY", "gemini-s3cret")]);
	assert_eq!(tyo.1["data"]["revision"], listed.1["data"]["revision"]);

	let sin = send(router.clone(), "GET", "/v1/nodes/sin/secrets", Some(SIN), None).await;
	let names: Vec<&str> = sin.1["data"]["items"]
		.as_array()
		.unwrap()
		.iter()
		.map(|item| item["name"].as_str().unwrap())
		.collect();
	assert_eq!(names, ["SHOT_KEY", "GROK_KEY"]);
	let refused = send(router, "GET", "/v1/nodes/sin/secrets", Some(TYO), None).await;
	assert_eq!(code(&refused), (StatusCode::UNAUTHORIZED, "invalid_token"));
}

#[tokio::test]
async fn a_node_is_told_when_no_registry_answers() {
	let Some(store) = store().await else { return };
	let seal = Arc::new(Seal::default());
	seal.open(RootKey::generate());
	let router = routes(secrets(store, seal, vec![nobody()]));
	let answer = send(router, "GET", "/v1/nodes/tyo/secrets", Some(TYO), None).await;
	assert_eq!(code(&answer), (StatusCode::BAD_GATEWAY, "upstream_unavailable"));
}

#[tokio::test]
async fn a_lone_core_with_no_share_checks_the_key_against_a_secret() {
	let Some(store) = store().await else { return };
	let key = RootKey::generate();
	let identity = Identity { scope: "platform", app: "gemini", name: "KEY" };
	store.set(&identity, "test", 0, &envelope::seal(&key, &identity, b"v"), |_| false).await.unwrap();
	let seal = Arc::new(Seal::default());
	let router = routes(secrets(store, seal.clone(), Vec::new()));
	let other = json!({ "key": *RootKey::generate().to_hex() });
	let answer = send(router.clone(), "POST", "/v1/seal", Some(TOKEN), Some(other)).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"));
	let right = json!({ "key": *key.to_hex() });
	assert_eq!(send(router, "POST", "/v1/seal", Some(TOKEN), Some(right)).await.0, StatusCode::OK);
	assert!(!seal.is_sealed());
}
