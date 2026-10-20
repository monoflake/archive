use super::*;
use crate::api::{
	self,
	tests::{PEER, nobody, secrets, serve},
};
use crate::seal::RootKey;
use crate::store::testing::unreached;

/// A core's secret service, sealed and holding `share`, served on loopback.
async fn core_holding(share: Option<Share>) -> String {
	let seal = Arc::new(Seal::default());
	if let Some(share) = share {
		seal.hold(share);
	}
	serve(api::routes(secrets(unreached(), seal, Control::default()))).await
}

fn scratch(name: &str) -> std::path::PathBuf {
	let directory = std::env::temp_dir().join(format!("secrets-peers-{}-{name}", std::process::id()));
	std::fs::create_dir_all(&directory).unwrap();
	directory
}

#[test]
fn the_cores_are_read_as_host_reads_them() {
	let cores = Cores::parse(" 100.64.0.1\n100.64.0.2 ").unwrap();
	assert_eq!(cores.peers, ["100.64.0.1", "100.64.0.2"].map(|host| format!("http://{host}:12014")));
	assert_eq!(Cores::parse("100.64.0.1 tyo"), Err("tyo".into()));
	assert_eq!(Cores::parse("").unwrap(), Cores::default());
}

#[test]
fn the_port_is_the_one_the_declaration_states() {
	let declaration = include_str!("../../service.toml");
	assert!(declaration.lines().any(|line| line.trim() == format!("port = {PORT}")));
}

#[tokio::test]
async fn two_sealed_cores_open_a_third_and_one_alone_does_not() {
	let key = RootKey::generate();
	let shares = seal::split(&key);
	let peers = vec![core_holding(Some(shares[1].clone())).await, core_holding(None).await, nobody()];
	let directory = scratch("open");
	let path = directory.join("share");
	std::fs::write(&path, shares[0].to_json().as_bytes()).unwrap();

	let seal = Arc::new(Seal::default());
	let every = Duration::from_millis(20);
	tokio::time::timeout(
		Duration::from_secs(10),
		unseal(seal.clone(), client(), peers, PEER.into(), path, every),
	)
	.await
	.expect("two shares open the lock");
	assert_eq!(seal.with_key(|held| *held.bytes()), Some(*key.bytes()));
	assert!(seal.share().unwrap() == shares[0]);

	let alone = Arc::new(Seal::default());
	let lone = vec![core_holding(None).await];
	let path = directory.join("share");
	let waited = tokio::time::timeout(
		Duration::from_millis(300),
		unseal(alone.clone(), client(), lone, PEER.into(), path, every),
	)
	.await;
	assert!(waited.is_err(), "one share opened the lock");
	assert!(alone.is_sealed());
}

#[tokio::test]
async fn a_core_with_no_file_is_opened_by_two_peers_and_a_wrong_token_by_none() {
	let key = RootKey::generate();
	let shares = seal::split(&key);
	let peers =
		vec![core_holding(Some(shares[1].clone())).await, core_holding(Some(shares[2].clone())).await];
	let missing = scratch("missing").join("share");
	let every = Duration::from_millis(20);

	let refused = Arc::new(Seal::default());
	let waited = tokio::time::timeout(
		Duration::from_millis(300),
		unseal(refused.clone(), client(), peers.clone(), "wrong".into(), missing.clone(), every),
	)
	.await;
	assert!(waited.is_err() && refused.is_sealed());

	let seal = Arc::new(Seal::default());
	tokio::time::timeout(
		Duration::from_secs(10),
		unseal(seal.clone(), client(), peers, PEER.into(), missing, every),
	)
	.await
	.expect("two peers open the lock");
	assert_eq!(seal.with_key(|held| *held.bytes()), Some(*key.bytes()));
	assert!(seal.share().is_none());
}

/// The order a first run takes: the directory made empty, the service started on it, and the share
/// written there by the ceremony while it runs.
#[tokio::test]
async fn a_share_written_into_an_empty_seal_is_read_without_a_restart() {
	let key = RootKey::generate();
	let shares = seal::split(&key);
	let peers = vec![core_holding(Some(shares[1].clone())).await];
	let path = scratch("late").join("share");
	let _ = std::fs::remove_file(&path);

	let seal = Arc::new(Seal::default());
	let every = Duration::from_millis(20);
	let asking =
		tokio::spawn(unseal(seal.clone(), client(), peers, PEER.into(), path.clone(), every));
	tokio::time::sleep(Duration::from_millis(200)).await;
	assert!(seal.is_sealed() && seal.share().is_none(), "opened with one share");

	std::fs::write(&path, shares[0].to_json().as_bytes()).unwrap();
	tokio::time::timeout(Duration::from_secs(10), asking)
		.await
		.expect("the share written was never read")
		.unwrap();
	assert_eq!(seal.with_key(|held| *held.bytes()), Some(*key.bytes()));
	assert!(seal.share().unwrap() == shares[0]);
}

#[tokio::test]
async fn a_share_file_that_is_not_one_is_not_held() {
	let path = scratch("bad").join("share");
	std::fs::write(&path, "{}").unwrap();
	assert!(read_share(&path).is_err());
	assert!(read_share(&path.with_file_name("absent")).is_err());
}

const TYO: &str = "0000000000000001";
const SIN: &str = "0000000000000009";
const NRT: &str = "0000000000000002";

#[test]
fn control_is_its_base_without_a_trailing_slash() {
	let host = "control.example";
	let control = Control::new(&format!(" https://{host}/\n"), " read \n");
	assert_eq!(control.base, format!("https://{host}"));
	assert_eq!(control.token, "read");
	assert_eq!(Control::new("", ""), Control::default());
}

/// Control answering every app whatever `?node=` asks, as a proxy dropping the query would.
async fn control_ignoring_the_node() -> String {
	use axum::routing::get;
	let items = serde_json::json!({ "items": [
		{ "kind": "app", "name": "gemini", "spec": { "placements": [TYO] } },
		{ "kind": "app", "name": "grok", "spec": { "placements": [SIN] } },
		{ "kind": "app", "name": "shot", "spec": { "placements": [TYO, SIN] } },
		{ "kind": "app", "name": "nowhere", "spec": {} },
	], "revision": 9 });
	let answer = move || {
		let items = items.clone();
		async move { response::success(axum::http::StatusCode::OK, items) }
	};
	serve(axum::Router::new().route("/v1/registry/apps", get(answer))).await
}

#[tokio::test]
async fn a_node_is_given_only_the_apps_placed_on_it_whatever_control_lists() {
	let control = Control::new(&control_ignoring_the_node().await, "read");
	let held = Placements::default();
	let placed = placed_apps(&client(), &control, &held, TYO).await;
	assert_eq!(placed, Some(vec!["gemini".to_owned(), "shot".to_owned()]));
	let placed = placed_apps(&client(), &control, &held, NRT).await;
	assert_eq!(placed, Some(Vec::new()));
	let silent = Control::new(&nobody(), "read");
	assert_eq!(placed_apps(&client(), &silent, &held, TYO).await, None);
	assert_eq!(placed_apps(&client(), &Control::default(), &held, TYO).await, None);
}

/// What a conditional control answers next, and every `If-None-Match` it was asked with.
#[derive(Default)]
struct Conditional {
	revision: u64,
	/// Each app's name and the nodes it is placed on.
	apps: Vec<(&'static str, Vec<&'static str>)>,
	/// Whether the tag is sent weakened, `W/"<revision>"`, as a proxy may.
	weak: bool,
	/// Whether every ask is answered `500`.
	failing: bool,
	asked: Vec<Option<String>>,
}

impl Conditional {
	fn tag(&self) -> String {
		let tag = format!("\"{}\"", self.revision);
		if self.weak { format!("W/{tag}") } else { tag }
	}
}

/// Control answering as its `GET /v1/registry/apps` does with tags: `ETag` on every `200`, and
/// `304` with the same tag when `If-None-Match` names the current one, to the tag sent alone.
async fn conditional_control(state: Arc<Mutex<Conditional>>) -> Control {
	use axum::http::{HeaderMap, StatusCode, header};
	use axum::response::IntoResponse;
	use axum::routing::get;
	let answer = move |headers: HeaderMap| {
		let state = state.clone();
		async move {
			let mut state = state.lock().unwrap();
			let asked = headers.get(header::IF_NONE_MATCH).map(|tag| tag.to_str().unwrap().to_owned());
			state.asked.push(asked.clone());
			if state.failing {
				return response::failure(StatusCode::INTERNAL_SERVER_ERROR, "store_unavailable");
			}
			let tag = state.tag();
			if asked.as_deref() == Some(tag.as_str()) {
				return (StatusCode::NOT_MODIFIED, [(header::ETAG, tag)]).into_response();
			}
			let items: Vec<Value> = state
				.apps
				.iter()
				.map(|(name, nodes)| serde_json::json!({ "name": name, "spec": { "placements": nodes } }))
				.collect();
			let body = serde_json::json!({ "items": items, "revision": state.revision });
			let mut answer = response::success(StatusCode::OK, body);
			answer.headers_mut().insert(header::ETAG, tag.parse().unwrap());
			answer
		}
	};
	let router = axum::Router::new().route("/v1/registry/apps", get(answer));
	Control::new(&serve(router).await, "read")
}

#[tokio::test]
async fn a_not_modified_is_answered_from_the_last_list_and_its_tag_sent_back_as_received() {
	let state = Arc::new(Mutex::new(Conditional {
		revision: 7,
		apps: vec![("gemini", vec![TYO]), ("grok", vec![SIN])],
		weak: true,
		..Conditional::default()
	}));
	let control = conditional_control(state.clone()).await;
	let held = Placements::default();
	let first = placed_apps(&client(), &control, &held, TYO).await;
	assert_eq!(first, Some(vec!["gemini".to_owned()]));
	// Control's list changing under the same revision shows the second answer is the held one.
	state.lock().unwrap().apps = Vec::new();
	let second = placed_apps(&client(), &control, &held, TYO).await;
	assert_eq!(second, first);
	// Each node holds its own tag: sin's first ask is unconditional.
	let sin = placed_apps(&client(), &control, &held, SIN).await;
	assert_eq!(sin, Some(Vec::new()));
	let asked = state.lock().unwrap().asked.clone();
	assert_eq!(asked, [None, Some("W/\"7\"".to_owned()), None]);
}

#[tokio::test]
async fn a_new_list_replaces_the_one_held_and_its_tag_is_sent_next() {
	let state = Arc::new(Mutex::new(Conditional {
		revision: 7,
		apps: vec![("gemini", vec![TYO])],
		..Conditional::default()
	}));
	let control = conditional_control(state.clone()).await;
	let held = Placements::default();
	assert_eq!(placed_apps(&client(), &control, &held, TYO).await, Some(vec!["gemini".to_owned()]));
	{
		let mut state = state.lock().unwrap();
		state.revision = 8;
		state.apps = vec![("gemini", vec![SIN]), ("shot", vec![TYO]), ("grok", vec![SIN])];
	}
	let replaced = placed_apps(&client(), &control, &held, TYO).await;
	assert_eq!(replaced, Some(vec!["shot".to_owned()]));
	let again = placed_apps(&client(), &control, &held, TYO).await;
	assert_eq!(again, replaced);
	let asked = state.lock().unwrap().asked.clone();
	assert_eq!(asked, [None, Some("\"7\"".to_owned()), Some("\"8\"".to_owned())]);
}

#[tokio::test]
async fn control_failing_is_a_refusal_whatever_is_held() {
	let state = Arc::new(Mutex::new(Conditional {
		revision: 7,
		apps: vec![("gemini", vec![TYO])],
		..Conditional::default()
	}));
	let control = conditional_control(state.clone()).await;
	let held = Placements::default();
	assert_eq!(placed_apps(&client(), &control, &held, TYO).await, Some(vec!["gemini".to_owned()]));
	state.lock().unwrap().failing = true;
	assert_eq!(placed_apps(&client(), &control, &held, TYO).await, None);
	// The same base gone silent is refused too, the list held never answered in its place.
	let silent = Control::new(&nobody(), "read");
	assert_eq!(placed_apps(&client(), &silent, &held, TYO).await, None);
	// Control back, the tag still held is sent and its answer taken.
	state.lock().unwrap().failing = false;
	assert_eq!(placed_apps(&client(), &control, &held, TYO).await, Some(vec!["gemini".to_owned()]));
	assert_eq!(state.lock().unwrap().asked.last(), Some(&Some("\"7\"".to_owned())));
}

#[tokio::test]
async fn a_control_without_tags_is_asked_in_full_every_time() {
	let control = Control::new(&control_ignoring_the_node().await, "read");
	let held = Placements::default();
	for _ in 0..2 {
		let placed = placed_apps(&client(), &control, &held, TYO).await;
		assert_eq!(placed, Some(vec!["gemini".to_owned(), "shot".to_owned()]));
	}
	assert!(held.held.lock().unwrap().is_empty());
}
