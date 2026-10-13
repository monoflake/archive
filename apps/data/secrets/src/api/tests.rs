use super::*;
use crate::seal::{self, Share};
use crate::store::testing::{store, unreached};
use axum::body::Body;
use axum::http::Request;
use http_body_util::BodyExt;
use tower::ServiceExt;

pub(crate) const TOKEN: &str = "write-s3cret";
pub(crate) const PEER: &str = "peer-s3cret";
/// A token that writes the `registry` app of the `accounts` scope alone.
const ACCOUNTS: &str = "accounts-s3cret";
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
		scopes: ScopeTokens::parse(&format!(
			"accounts/registry:{}",
			hex::encode(tokens::digest(ACCOUNTS))
		))
		.unwrap(),
		peer_token: PEER.into(),
		nodes: node_tokens(),
		node_scopes: vec!["platform".into(), "canmi".into()],
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

/// A registry that places `gemini`, `shot` and `registry` on `tyo`, and `grok` and `shot` on `sin`,
/// answering as the registry's `GET /v1/apps?node=` does.
async fn fake_registry() -> String {
	#[derive(serde::Deserialize)]
	struct Asked {
		node: Option<String>,
	}
	let placed = [
		("gemini", ["tyo"].as_slice()),
		("grok", &["sin"]),
		("shot", &["tyo", "sin"]),
		("registry", &["tyo"]),
	];
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
async fn writes_take_the_write_token_or_a_scope_token() {
	let closed = Secrets {
		token: String::new(),
		scopes: ScopeTokens::default(),
		..Arc::into_inner(secrets(unreached(), Arc::default(), Vec::new())).unwrap()
	};
	let closed = routes(Arc::new(closed));
	let answer =
		send(closed.clone(), "PUT", "/v1/secrets/gemini/KEY", Some(TOKEN), Some(wanted(0, "v"))).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
	let answer = send(closed, "GET", "/v1/secrets", Some(TOKEN), None).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));

	// Without the write token, a scope's token still writes its scope.
	let scoped_only = Secrets {
		token: String::new(),
		..Arc::into_inner(secrets(unreached(), Arc::default(), Vec::new())).unwrap()
	};
	let scoped_only = routes(Arc::new(scoped_only));
	let answer = send(scoped_only.clone(), "GET", "/v1/secrets", Some(TOKEN), None).await;
	assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"));
	let answer = send(scoped_only, "GET", "/v1/secrets", Some(ACCOUNTS), None).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));

	let (router, _) = refusing();
	let compare = json!({ "by": "test", "value": "v" });
	for bearer in [None, Some(""), Some("write-s3cre"), Some(PEER), Some(TYO), Some("accounts-s3cre")]
	{
		let answer =
			send(router.clone(), "PUT", "/v1/secrets/gemini/KEY", bearer, Some(wanted(0, "v"))).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
		let answer =
			send(router.clone(), "POST", "/v1/seal", bearer, Some(json!({ "key": "00" }))).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
		let answer = send(router.clone(), "GET", "/v1/secrets", bearer, None).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
		let answer = send(router.clone(), "GET", "/v1/secrets/gemini/KEY", bearer, None).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
		let path = "/v1/secrets/gemini/KEY/compare";
		let answer = send(router.clone(), "POST", path, bearer, Some(compare.clone())).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{bearer:?}");
	}
	// A scope's token never opens the lock.
	let answer =
		send(router.clone(), "POST", "/v1/seal", Some(ACCOUNTS), Some(json!({ "key": "00" }))).await;
	assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"));
}

#[tokio::test]
async fn a_scope_token_reaches_its_own_app_of_its_own_scope_alone() {
	let (router, _) = refusing();
	let compare = json!({ "by": "test", "value": "v" });
	for path in [
		"/v1/secrets/registry/KEY",
		"/v1/secrets/registry/KEY?scope=canmi",
		"/v1/secrets/web/KEY?scope=accounts",
		"/v1/secrets/gemini/KEY?scope=accounts",
	] {
		let answer = send(router.clone(), "PUT", path, Some(ACCOUNTS), Some(wanted(0, "v"))).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{path}");
		let answer = send(router.clone(), "GET", path, Some(ACCOUNTS), None).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{path}");
	}
	for path in [
		"/v1/secrets/registry/KEY/compare?scope=platform",
		"/v1/secrets/web/KEY/compare?scope=accounts",
	] {
		let answer = send(router.clone(), "POST", path, Some(ACCOUNTS), Some(compare.clone())).await;
		assert_eq!(code(&answer), (StatusCode::UNAUTHORIZED, "invalid_token"), "{path}");
	}
	// In its own scope it is let through, to the seal.
	let path = "/v1/secrets/registry/KEY?scope=accounts";
	let answer = send(router.clone(), "PUT", path, Some(ACCOUNTS), Some(wanted(0, "v"))).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
	let path = "/v1/secrets/registry/KEY/compare?scope=accounts";
	let answer = send(router, "POST", path, Some(ACCOUNTS), Some(compare)).await;
	assert_eq!(code(&answer), (StatusCode::SERVICE_UNAVAILABLE, "service_unavailable"));
}

#[tokio::test]
async fn a_scope_token_never_writes_what_changes_how_a_process_starts() {
	let seal = Arc::new(Seal::default());
	seal.open(RootKey::generate());
	let router = routes(secrets(unreached(), seal, Vec::new()));
	let refused = [
		"LD_PRELOAD",
		"LD_LIBRARY_PATH",
		"DYLD_INSERT_LIBRARIES",
		"NODE_OPTIONS",
		"NODE_PATH",
		"PYTHONPATH",
		"PYTHONSTARTUP",
		"PERL5OPT",
		"RUBYOPT",
		"BASH_ENV",
		"ENV",
		"PATH",
		"JAVA_TOOL_OPTIONS",
		"_JAVA_OPTIONS",
		"SSL_CERT_FILE",
	];
	for name in refused {
		for body in [wanted(0, "/tmp/x.so"), reclassed(1, "secret")] {
			let path = format!("/v1/secrets/registry/{name}?scope=accounts");
			let answer = send(router.clone(), "PUT", &path, Some(ACCOUNTS), Some(body)).await;
			assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "forbidden_field"), "{name}");
			assert!(answer.1["message"].as_str().unwrap().contains(name), "{}", answer.1);
		}
		// The write token writes them, here as far as the store, which this router never reaches.
		let path = format!("/v1/secrets/registry/{name}?scope=accounts");
		let answer = send(router.clone(), "PUT", &path, Some(TOKEN), Some(wanted(0, "v"))).await;
		assert_eq!(code(&answer), (StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable"), "{name}");
	}
	// Names that only look alike are the scope token's to write.
	for name in ["PATHS", "ENVIRONMENT", "NODE_ENV", "MY_LD_PRELOAD", "IDENTITY"] {
		let path = format!("/v1/secrets/registry/{name}?scope=accounts");
		let answer = send(router.clone(), "PUT", &path, Some(ACCOUNTS), Some(wanted(0, "v"))).await;
		assert_eq!(code(&answer), (StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable"), "{name}");
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
		json!({
			"scope": "canmi", "app": "shot", "name": "SHOT_KEY", "class": "secret",
			"revision": items[0]["revision"], "changed": items[0]["changed"],
		})
	);
	assert!(items[0]["changed"].is_string());

	let tyo = send(router.clone(), "GET", "/v1/nodes/tyo/secrets", Some(TYO), None).await;
	assert_eq!(tyo.0, StatusCode::OK, "{}", tyo.1);
	let values: Vec<(&str, &str)> = tyo.1["data"]["items"]
		.as_array()
		.unwrap()
		.iter()
		.map(|item| (item["name"].as_str().unwrap(), item["value"].as_str().unwrap()))
		.collect();
	assert_eq!(values, [("SHOT_KEY", "shot-s3cret"), ("GEMINI_KEY", "gemini-s3cret")]);
	assert!(tyo.1["data"]["items"].as_array().unwrap().iter().all(|item| item["class"] == "secret"));
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
	let sealed = envelope::seal(&key, &identity, b"v");
	store.set(&identity, "test", 0, Wanted::Value(&sealed, Class::Secret), |_| false).await.unwrap();
	let seal = Arc::new(Seal::default());
	let router = routes(secrets(store, seal.clone(), Vec::new()));
	let other = json!({ "key": *RootKey::generate().to_hex() });
	let answer = send(router.clone(), "POST", "/v1/seal", Some(TOKEN), Some(other)).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"));
	let right = json!({ "key": *key.to_hex() });
	assert_eq!(send(router, "POST", "/v1/seal", Some(TOKEN), Some(right)).await.0, StatusCode::OK);
	assert!(!seal.is_sealed());
}

/// A router over the test database, unsealed, and the service behind it.
async fn opened() -> Option<(Router, Shared)> {
	let store = store().await?;
	let seal = Arc::new(Seal::default());
	seal.open(RootKey::generate());
	let shared = secrets(store, seal, vec![fake_registry().await]);
	Some((routes(shared.clone()), shared))
}

fn classed(revision: i64, value: &str, class: &str) -> Value {
	json!({ "by": "test", "revision": revision, "value": value, "class": class })
}

fn reclassed(revision: i64, class: &str) -> Value {
	json!({ "by": "test", "revision": revision, "class": class })
}

async fn put(router: &Router, path: &str, bearer: &str, body: Value) -> (StatusCode, Value) {
	send(router.clone(), "PUT", path, Some(bearer), Some(body)).await
}

fn revision_of(answer: &(StatusCode, Value)) -> i64 {
	answer.1["data"]["revision"].as_i64().unwrap_or_else(|| panic!("{}", answer.1))
}

#[tokio::test]
async fn a_config_is_read_whole_and_a_secret_never() {
	let Some((router, _)) = opened().await else { return };
	let config =
		put(&router, "/v1/secrets/gemini/MODEL", TOKEN, classed(0, "gemini-3", "config")).await;
	let config = revision_of(&config);
	let secret = put(&router, "/v1/secrets/gemini/KEY", TOKEN, wanted(0, "gemini-s3cret")).await;
	revision_of(&secret);

	let listed = send(router.clone(), "GET", "/v1/secrets", Some(TOKEN), None).await;
	assert!(!listed.1.to_string().contains("s3cret"), "{}", listed.1);
	let items = listed.1["data"]["items"].as_array().unwrap();
	let shown: Vec<(&str, &str, Option<&str>)> = items
		.iter()
		.map(|item| {
			(item["name"].as_str().unwrap(), item["class"].as_str().unwrap(), item["value"].as_str())
		})
		.collect();
	assert_eq!(shown, [("KEY", "secret", None), ("MODEL", "config", Some("gemini-3"))]);

	let read = send(router.clone(), "GET", "/v1/secrets/gemini/MODEL", Some(TOKEN), None).await;
	assert_eq!((read.0, read.1["data"]["value"].as_str()), (StatusCode::OK, Some("gemini-3")));
	assert_eq!(read.1["data"]["revision"], config);
	let read = send(router.clone(), "GET", "/v1/secrets/gemini/KEY", Some(TOKEN), None).await;
	assert_eq!(read.0, StatusCode::OK);
	assert!(read.1["data"].get("value").is_none(), "{}", read.1);
	assert_eq!(read.1["data"]["class"], "secret");
	let missing = send(router.clone(), "GET", "/v1/secrets/gemini/NONE", Some(TOKEN), None).await;
	assert_eq!(code(&missing), (StatusCode::NOT_FOUND, "no_such_resource"));

	// The node it is placed on is answered both, each with its class.
	let tyo = send(router, "GET", "/v1/nodes/tyo/secrets", Some(TYO), None).await;
	let given: Vec<(&str, &str, &str)> = tyo.1["data"]["items"]
		.as_array()
		.unwrap()
		.iter()
		.map(|item| {
			let field = |key: &str| item[key].as_str().unwrap();
			(field("name"), field("class"), field("value"))
		})
		.collect();
	assert_eq!(given, [("KEY", "secret", "gemini-s3cret"), ("MODEL", "config", "gemini-3")]);
}

#[tokio::test]
async fn a_secret_shows_only_written_again_and_a_config_hides_by_its_class() {
	let Some((router, _)) = opened().await else { return };
	let path = "/v1/secrets/gemini/KEY";
	let created = revision_of(&put(&router, path, TOKEN, wanted(0, "s3cret")).await);
	let shown = put(&router, path, TOKEN, reclassed(created, "config")).await;
	assert_eq!(code(&shown), (StatusCode::BAD_REQUEST, "forbidden_field"));
	let read = send(router.clone(), "GET", path, Some(TOKEN), None).await;
	assert_eq!(
		(read.1["data"]["class"].as_str(), read.1["data"].get("value")),
		(Some("secret"), None)
	);

	let rewritten = put(&router, path, TOKEN, classed(created, "s3cret", "config")).await;
	let rewritten = revision_of(&rewritten);
	let read = send(router.clone(), "GET", path, Some(TOKEN), None).await;
	assert_eq!(read.1["data"]["value"], "s3cret");

	let hidden = revision_of(&put(&router, path, TOKEN, reclassed(rewritten, "secret")).await);
	assert!(hidden > rewritten);
	let read = send(router.clone(), "GET", path, Some(TOKEN), None).await;
	assert_eq!(
		(read.1["data"]["class"].as_str(), read.1["data"].get("value")),
		(Some("secret"), None)
	);
	let again = put(&router, path, TOKEN, reclassed(hidden, "secret")).await;
	assert_eq!((again.0, &again.1["data"]), (StatusCode::OK, &json!({})));

	// A write naming no class is a secret, whatever was kept.
	let config = revision_of(&put(&router, path, TOKEN, classed(hidden, "v2", "config")).await);
	let unnamed = revision_of(&put(&router, path, TOKEN, wanted(config, "v2")).await);
	let read = send(router.clone(), "GET", path, Some(TOKEN), None).await;
	assert_eq!(
		(read.1["data"]["class"].as_str(), read.1["data"]["revision"].as_i64()),
		(Some("secret"), Some(unnamed))
	);

	let missing = put(&router, "/v1/secrets/gemini/NONE", TOKEN, reclassed(0, "secret")).await;
	assert_eq!(code(&missing), (StatusCode::NOT_FOUND, "no_such_resource"));
	for body in [
		json!({ "by": "test", "revision": 0, "class": "public" }),
		json!({ "by": "test", "revision": 0, "value": "v", "class": 1 }),
	] {
		let answer = put(&router, "/v1/secrets/gemini/OTHER", TOKEN, body.clone()).await;
		assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "invalid_body"), "{body}");
	}
}

#[tokio::test]
async fn a_secret_is_compared_unread_and_each_comparison_is_kept() {
	let Some((router, shared)) = opened().await else { return };
	put(&router, "/v1/secrets/gemini/KEY", TOKEN, wanted(0, "gemini-s3cret")).await;
	put(&router, "/v1/secrets/gemini/MODEL", TOKEN, classed(0, "gemini-3", "config")).await;
	let compare = |name: &'static str, value: &'static str| {
		let router = router.clone();
		async move {
			let path = format!("/v1/secrets/gemini/{name}/compare");
			send(router, "POST", &path, Some(TOKEN), Some(json!({ "by": "frank", "value": value }))).await
		}
	};

	let right = compare("KEY", "gemini-s3cret").await;
	assert_eq!((right.0, &right.1["data"]), (StatusCode::OK, &json!({ "matches": true })));
	let wrong = compare("KEY", "gemini-s3cre").await;
	assert_eq!((wrong.0, &wrong.1["data"]), (StatusCode::OK, &json!({ "matches": false })));
	let longer = compare("KEY", "gemini-s3cret-and-more").await;
	assert_eq!(longer.1["data"], json!({ "matches": false }));
	assert!(!right.1.to_string().contains("s3cret") && !wrong.1.to_string().contains("s3cret"));

	let config = compare("MODEL", "gemini-3").await;
	assert_eq!(code(&config), (StatusCode::BAD_REQUEST, "invalid_body"));
	let missing = compare("NONE", "x").await;
	assert_eq!(code(&missing), (StatusCode::NOT_FOUND, "no_such_resource"));
	let extra = json!({ "by": "frank", "value": "x", "revision": 1 });
	let path = "/v1/secrets/gemini/KEY/compare";
	let answer = send(router.clone(), "POST", path, Some(TOKEN), Some(extra)).await;
	assert_eq!(code(&answer), (StatusCode::BAD_REQUEST, "forbidden_field"));

	// Three asked so far of KEY; two more are answered, and the sixth is refused, uncounted.
	compare("KEY", "a").await;
	compare("KEY", "b").await;
	let request = Request::builder()
		.method("POST")
		.uri(path)
		.header("authorization", format!("Bearer {TOKEN}"))
		.header("content-type", "application/json")
		.body(Body::from(json!({ "by": "frank", "value": "gemini-s3cret" }).to_string()))
		.unwrap();
	let refused = router.clone().oneshot(request).await.unwrap();
	assert_eq!(refused.status(), StatusCode::TOO_MANY_REQUESTS);
	let wait: u64 = refused.headers()[RETRY_AFTER].to_str().unwrap().parse().unwrap();
	assert!((1..=60).contains(&wait), "{wait}");
	let bytes = refused.into_body().collect().await.unwrap().to_bytes();
	let refused: Value = serde_json::from_slice(&bytes).unwrap();
	assert_eq!(refused["code"], "rate_limited");

	let kept = crate::store::testing::comparisons(&shared.store).await;
	assert_eq!(
		kept,
		[
			("frank".to_owned(), "KEY".to_owned(), true),
			("frank".into(), "KEY".into(), false),
			("frank".into(), "KEY".into(), false),
			("frank".into(), "KEY".into(), false),
			("frank".into(), "KEY".into(), false),
		]
	);
	let listed = send(router, "GET", "/v1/secrets", Some(TOKEN), None).await;
	assert_eq!(listed.1["data"]["revision"], 2, "a comparison moved a revision");
}

#[tokio::test]
async fn a_scope_token_writes_lists_and_compares_its_own_app_alone() {
	let Some((router, _)) = opened().await else { return };
	let own = "/v1/secrets/registry/IDENTITY?scope=accounts";
	let written = put(&router, own, ACCOUNTS, wanted(0, "identity-s3cret")).await;
	revision_of(&written);
	let neighbor = "/v1/secrets/web/KEY?scope=accounts";
	revision_of(&put(&router, neighbor, TOKEN, wanted(0, "web-s3cret")).await);
	put(&router, "/v1/secrets/gemini/KEY", TOKEN, wanted(0, "gemini-s3cret")).await;
	put(&router, "/v1/secrets/shot/KEY?scope=canmi", TOKEN, wanted(0, "shot-s3cret")).await;

	let scoped = send(router.clone(), "GET", "/v1/secrets", Some(ACCOUNTS), None).await;
	let names: Vec<(&str, &str)> = scoped.1["data"]["items"]
		.as_array()
		.unwrap()
		.iter()
		.map(|item| (item["scope"].as_str().unwrap(), item["name"].as_str().unwrap()))
		.collect();
	assert_eq!(names, [("accounts", "IDENTITY")]);
	assert_eq!(scoped.1["data"]["revision"], 1, "a scope token's list told of another's change");
	let every = send(router.clone(), "GET", "/v1/secrets", Some(TOKEN), None).await;
	assert_eq!(every.1["data"]["items"].as_array().unwrap().len(), 4);
	let neighbor_read = send(router.clone(), "GET", neighbor, Some(ACCOUNTS), None).await;
	assert_eq!(code(&neighbor_read), (StatusCode::UNAUTHORIZED, "invalid_token"));
	let overwritten = put(&router, neighbor, ACCOUNTS, wanted(1, "x")).await;
	assert_eq!(code(&overwritten), (StatusCode::UNAUTHORIZED, "invalid_token"));

	let path = "/v1/secrets/registry/IDENTITY/compare?scope=accounts";
	let body = json!({ "by": "registry", "value": "identity-s3cret" });
	let compared = send(router.clone(), "POST", path, Some(ACCOUNTS), Some(body)).await;
	assert_eq!(compared.1["data"], json!({ "matches": true }));
	let read = send(router.clone(), "GET", own, Some(ACCOUNTS), None).await;
	assert_eq!(read.0, StatusCode::OK);
	assert!(read.1["data"].get("value").is_none());

	let elsewhere = send(router.clone(), "GET", "/v1/secrets/gemini/KEY", Some(ACCOUNTS), None).await;
	assert_eq!(code(&elsewhere), (StatusCode::UNAUTHORIZED, "invalid_token"));
	let body = json!({ "by": "registry", "value": "gemini-s3cret" });
	let path = "/v1/secrets/gemini/KEY/compare";
	let elsewhere = send(router, "POST", path, Some(ACCOUNTS), Some(body)).await;
	assert_eq!(code(&elsewhere), (StatusCode::UNAUTHORIZED, "invalid_token"));
}

#[test]
fn a_value_is_compared_whole_whatever_its_length() {
	assert!(same_value(b"s3cret", b"s3cret"));
	assert!(!same_value(b"s3cret", b"s3creT"));
	assert!(!same_value(b"s3cret", b"s3cre"));
	assert!(!same_value(b"s3cret", b"s3cret\0"));
	assert!(same_value(b"", b""));
}

#[test]
fn the_scopes_a_node_is_given_are_platform_unless_named() {
	assert_eq!(node_scopes("").unwrap(), ["platform"]);
	assert_eq!(node_scopes("  \n").unwrap(), ["platform"]);
	assert_eq!(node_scopes("platform canmi").unwrap(), ["platform", "canmi"]);
	assert_eq!(node_scopes("platform Accounts"), Err("Accounts".into()));
}

#[tokio::test]
async fn a_held_scope_is_never_given_to_a_node_whatever_app_it_names() {
	let Some(store) = store().await else { return };
	let seal = Arc::new(Seal::default());
	seal.open(RootKey::generate());
	let shared = Secrets {
		node_scopes: node_scopes("").unwrap(),
		..Arc::into_inner(secrets(store, seal, vec![fake_registry().await])).unwrap()
	};
	let router = routes(Arc::new(shared));
	// `registry` is placed on tyo, and its account identities are kept under `accounts`.
	let held = "/v1/secrets/registry/IDENTITY?scope=accounts";
	revision_of(&put(&router, held, ACCOUNTS, wanted(0, "identity-s3cret")).await);
	let canmi = "/v1/secrets/registry/CANMI_KEY?scope=canmi";
	revision_of(&put(&router, canmi, TOKEN, wanted(0, "canmi-s3cret")).await);
	revision_of(&put(&router, "/v1/secrets/registry/REGISTRY_TOKEN", TOKEN, wanted(0, "r")).await);

	let tyo = send(router, "GET", "/v1/nodes/tyo/secrets", Some(TYO), None).await;
	assert_eq!(tyo.0, StatusCode::OK, "{}", tyo.1);
	assert!(!tyo.1.to_string().contains("identity-s3cret"), "{}", tyo.1);
	let given: Vec<(&str, &str)> = tyo.1["data"]["items"]
		.as_array()
		.unwrap()
		.iter()
		.map(|item| (item["scope"].as_str().unwrap(), item["name"].as_str().unwrap()))
		.collect();
	assert_eq!(given, [("platform", "REGISTRY_TOKEN")]);
}
