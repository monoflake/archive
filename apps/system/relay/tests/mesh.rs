//! Relays on loopback ports, each reading a fake host, holding sockets to each other as they do
//! across the tailnet.

use axum::serve::Listener;
use bytes::Bytes;
use futures_util::StreamExt;
use http_body_util::{BodyExt, Empty};
use hyper::body::Incoming;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use relay::config::Peer;
use relay::host::{App, Event, PAGE, ReadError, Reader, Reading, Source};
use relay::presence::{Presence, sweep};
use relay::relay::{Relay, recall, watch};
use relay::runs::Store;
use relay::socket::Socket;
use std::future::{Future, IntoFuture};
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio_tungstenite::MaybeTlsStream;
use tokio_tungstenite::tungstenite::Message;

const SECRET: &str = "s3cret";
const ROUND: Duration = Duration::from_millis(20);
const PATIENCE: Duration = Duration::from_secs(10);

/// A host running one app, `geo`, at an image the test changes, and the events the test writes.
struct Fake {
	image: Mutex<String>,
	events: Mutex<Vec<Event>>,
}

impl Fake {
	fn deploy(&self, image: &str) {
		*self.image.lock().unwrap() = image.to_owned();
	}

	/// An event of `id` written, or rewritten with its outcome moved on.
	fn record(&self, id: i64, outcome: &str) {
		self.write(id, "geo", outcome);
	}

	/// A deploy of the relay itself, under way.
	fn upgrading(&self) {
		self.write(1000, relay::leaving::APP, "running");
	}

	fn write(&self, id: i64, app: &str, outcome: &str) {
		let event = Event {
			id,
			app: app.into(),
			action: "deploy".into(),
			source: Source { kind: "panel".into(), run: None, commit: None },
			image: None,
			outcome: outcome.into(),
			stage: None,
			detail: None,
			started_at: jiff::Timestamp::now().to_string(),
			finished_at: None,
		};
		let mut events = self.events.lock().unwrap();
		events.retain(|written| written.id != id);
		events.push(event);
		events.sort_by_key(|event| -event.id);
	}

	fn before(&self, before: Option<i64>) -> Vec<Event> {
		let events = self.events.lock().unwrap();
		let older = events.iter().filter(|event| before.is_none_or(|before| event.id < before));
		older.take(PAGE).cloned().collect()
	}
}

impl Reader for Fake {
	fn read(&self) -> Pin<Box<dyn Future<Output = Reading> + Send + '_>> {
		let image = self.image.lock().unwrap().clone();
		let events = self.before(None);
		Box::pin(async move {
			let app = App {
				name: "geo".into(),
				image,
				deployed_at: "2026-10-06T12:00:00Z".into(),
				running: true,
				held: false,
				rollout: "beside".into(),
				label: None,
				driver: false,
			};
			// The meter's second moves every round; nothing a snapshot says does.
			let second = jiff::Timestamp::now().as_second();
			let machine = serde_json::json!({ "info": { "memory": 1000 },
				"sample": { "at": second, "values": { "cpu.usage": 5.0 } } });
			Reading { events: Ok(events), apps: Ok(vec![app]), machine: Ok(machine) }
		})
	}

	fn page(
		&self,
		before: Option<i64>,
	) -> Pin<Box<dyn Future<Output = Result<Vec<Event>, ReadError>> + Send + '_>> {
		let events = self.before(before);
		Box::pin(async move { Ok(events) })
	}
}

struct Started {
	relay: Arc<Relay>,
	host: Arc<Fake>,
	address: SocketAddr,
}

async fn start(node: &str, image: &str) -> Started {
	let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
	let address = listener.local_addr().unwrap();
	let listener = relay::mesh::accepting(listener);
	let relay =
		Relay::new(node.into(), SECRET.into(), Default::default(), Store::memory().unwrap()).unwrap();
	let host = Arc::new(Fake { image: Mutex::new(image.into()), events: Mutex::default() });
	tokio::spawn(axum::serve(listener, relay::api::routes(relay.clone())).into_future());
	tokio::spawn(watch(relay.clone(), host.clone(), ROUND));
	tokio::spawn(recall(relay.clone(), host.clone()));
	tokio::spawn(sweep(relay.clone(), ROUND));
	Started { relay, host, address }
}

fn hold(from: &Started, to: &Started) {
	let peer = Peer { id: to.relay.node().into(), address: to.address.to_string() };
	tokio::spawn(relay::mesh::keep(from.relay.clone(), peer));
}

/// The image `relay` holds for `node`'s one app.
fn image(relay: &Relay, node: &str) -> Option<String> {
	let state = relay.state();
	let snapshot = &state.nodes.get(node)?.held.as_ref()?.snapshot;
	snapshot["apps"][0]["image"].as_str().map(str::to_owned)
}

async fn until(what: &str, mut done: impl FnMut() -> bool) {
	let waited = async {
		while !done() {
			tokio::time::sleep(ROUND).await;
		}
	};
	tokio::time::timeout(PATIENCE, waited).await.unwrap_or_else(|_| panic!("never: {what}"));
}

#[tokio::test]
async fn two_relays_converge_after_one_changes() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:7").await;
	hold(&rdu, &tyo);
	hold(&tyo, &rdu);
	until("each holds the other", || {
		image(&rdu.relay, "0000000000000001").as_deref() == Some("geo:7")
			&& image(&tyo.relay, "0000000000000007").as_deref() == Some("geo:1")
	})
	.await;

	let (mut browser, _) =
		tokio_tungstenite::connect_async(format!("ws://{}/live", rdu.address)).await.unwrap();
	let first = text(&mut browser).await;
	assert_eq!((&first["type"], &first["node"]), (&"cluster".into(), &"0000000000000007".into()));
	assert_eq!(first["version"], 2);
	assert_eq!(first["nodes"]["0000000000000001"]["snapshot"]["apps"][0]["image"], "geo:7");
	let before = first["nodes"]["0000000000000001"]["version"].as_u64().unwrap();

	tyo.host.deploy("geo:8");
	until("rdu hears tyo's change", || {
		image(&rdu.relay, "0000000000000001").as_deref() == Some("geo:8")
	})
	.await;
	// tyo's round trips change its snapshot too, so one sent before the deploy may come first.
	loop {
		let next = text(&mut browser).await;
		let image = &next["state"]["snapshot"]["apps"][0]["image"];
		if next["type"] == "node" && next["node"] == "0000000000000001" && image == "geo:8" {
			assert!(next["state"]["version"].as_u64().unwrap() > before);
			break;
		}
	}
	// Neither holds anything of the other's as its own.
	assert_eq!(image(&tyo.relay, "0000000000000001").as_deref(), Some("geo:8"));
	assert_eq!(image(&rdu.relay, "0000000000000007").as_deref(), Some("geo:1"));
}

#[tokio::test]
async fn a_relay_that_compresses_and_one_that_does_not_still_hear_each_other() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:7").await;
	// tyo stands in for a relay older than compressed frames: it neither says it reads them nor
	// sends them, so each end speaks to the other plain.
	tyo.relay.set_compression(false);
	hold(&rdu, &tyo);
	hold(&tyo, &rdu);
	until("each holds the other", || {
		image(&rdu.relay, "0000000000000001").as_deref() == Some("geo:7")
			&& image(&tyo.relay, "0000000000000007").as_deref() == Some("geo:1")
	})
	.await;
	rdu.host.deploy("geo:2");
	tyo.host.deploy("geo:8");
	until("each hears the other's change", || {
		image(&rdu.relay, "0000000000000001").as_deref() == Some("geo:8")
			&& image(&tyo.relay, "0000000000000007").as_deref() == Some("geo:2")
	})
	.await;
}

#[tokio::test]
async fn a_relay_that_syncs_and_one_that_compares_still_hear_each_other() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:7").await;
	let buf = start("0000000000000006", "geo:3").await;
	// tyo stands in for a relay older than the sync, and buf reaches rdu only through it.
	tyo.relay.set_sync(false);
	hold(&rdu, &tyo);
	hold(&tyo, &rdu);
	hold(&buf, &tyo);
	until("rdu hears tyo and buf", || {
		image(&rdu.relay, "0000000000000001").as_deref() == Some("geo:7")
			&& image(&rdu.relay, "0000000000000006").as_deref() == Some("geo:3")
	})
	.await;
	rdu.host.deploy("geo:2");
	tyo.host.deploy("geo:8");
	buf.host.deploy("geo:4");
	until("each hears the others' changes", || {
		image(&rdu.relay, "0000000000000001").as_deref() == Some("geo:8")
			&& image(&rdu.relay, "0000000000000006").as_deref() == Some("geo:4")
			&& image(&tyo.relay, "0000000000000007").as_deref() == Some("geo:2")
			&& image(&buf.relay, "0000000000000007").as_deref() == Some("geo:2")
	})
	.await;
}

#[tokio::test]
async fn a_browser_that_asks_for_deltas_is_sent_what_changed_of_a_node() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:7").await;
	hold(&rdu, &tyo);
	hold(&tyo, &rdu);
	until("rdu holds tyo", || image(&rdu.relay, "0000000000000001").as_deref() == Some("geo:7"))
		.await;
	let url = format!("ws://{}/live?deltas=true", rdu.address);
	let (mut browser, _) = tokio_tungstenite::connect_async(url).await.unwrap();
	let first = text(&mut browser).await;
	let before = first["nodes"]["0000000000000001"]["version"].as_u64().unwrap();
	tyo.host.deploy("geo:8");
	loop {
		let next = text(&mut browser).await;
		assert_ne!(next["type"], "node", "an entry whole, to a socket that asked for deltas: {next}");
		let rows = &next["snapshot"]["apps"]["set"];
		if next["type"] == "delta" && next["node"] == "0000000000000001" && rows[0]["image"] == "geo:8"
		{
			assert_eq!(next["base"].as_u64(), Some(before));
			assert!(next["set"]["version"].as_u64().unwrap() > before);
			break;
		}
	}
}

#[tokio::test]
async fn a_node_reaches_a_third_through_a_neighbor() {
	// rdu and buf know tyo alone, and tyo dials nobody: it only accepts.
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:2").await;
	let buf = start("0000000000000006", "geo:3").await;
	hold(&rdu, &tyo);
	hold(&buf, &tyo);
	until("rdu hears buf", || image(&rdu.relay, "0000000000000006").as_deref() == Some("geo:3"))
		.await;
	buf.host.deploy("geo:4");
	until("rdu hears buf's change", || {
		image(&rdu.relay, "0000000000000006").as_deref() == Some("geo:4")
	})
	.await;
	until("buf hears rdu", || image(&buf.relay, "0000000000000007").as_deref() == Some("geo:1"))
		.await;
}

#[tokio::test]
async fn each_relay_times_the_other_and_shows_its_own_round_trips() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:2").await;
	hold(&rdu, &tyo);
	until("each times the other", || {
		rdu.relay.round_trips().contains_key("0000000000000001")
			&& tyo.relay.round_trips().contains_key("0000000000000007")
	})
	.await;
	let round_trip = rdu.relay.round_trips()["0000000000000001"];
	assert!((0.0..1.0).contains(&round_trip), "{round_trip}");
	// Its own as timed now; another node's from its newest minute, which none has ended yet.
	let state = rdu.relay.state();
	assert!(
		state.nodes["0000000000000007"]
			.round_trip
			.as_ref()
			.is_some_and(|trips| trips.contains_key("0000000000000001"))
	);
	assert!(state.nodes.get("0000000000000001").is_none_or(|tyo| tyo.round_trip.is_none()));
	// A snapshot carries neither the round trips nor the meter's second.
	let snapshot = &state.nodes["0000000000000007"].held.as_ref().unwrap().snapshot;
	assert!(snapshot.get("round_trip").is_none());
	assert_eq!(snapshot["machine"], serde_json::json!({ "info": { "memory": 1000 } }));
}

#[tokio::test]
async fn a_node_is_heard_by_its_beats_while_its_version_stands_still() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:2").await;
	hold(&rdu, &tyo);
	hold(&tyo, &rdu);
	until("rdu holds tyo", || image(&rdu.relay, "0000000000000001").is_some()).await;
	let held = |relay: &Relay| relay.state().nodes["0000000000000001"].held.clone().unwrap();
	let before = held(&rdu.relay);
	until("tyo heard again", || held(&rdu.relay).heard_at > before.heard_at).await;
	tokio::time::sleep(ROUND * 10).await;
	let after = held(&rdu.relay);
	assert_eq!(after.version, before.version, "no new version while nothing changed");
	assert!(after.heard_at > before.heard_at);
	assert_eq!(rdu.relay.state().nodes["0000000000000001"].state, Presence::Live);
}

#[tokio::test]
async fn a_stream_watching_every_node_is_sent_their_fine_readings_until_it_closes() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:2").await;
	hold(&rdu, &tyo);
	hold(&tyo, &rdu);
	until("each speaks to the other", || {
		rdu.relay.speaking().contains("0000000000000001")
			&& tyo.relay.speaking().contains("0000000000000007")
	})
	.await;
	let (_, mut events) = Events::open(rdu.address, "?deltas=true&watch=all").await;
	let seconds = async {
		loop {
			let next = events.next().await;
			assert_ne!(next["type"], "readings", "a page is sent a round's readings, never one a node");
			let seconds = &next["seconds"];
			if next["type"] == "seconds" && seconds.get("0000000000000001").is_some() {
				assert_eq!(seconds["0000000000000001"]["values"]["cpu.usage"], 5.0);
				return;
			}
		}
	};
	tokio::time::timeout(PATIENCE, seconds).await.expect("tyo's readings in time");
	assert!(rdu.relay.watched() && tyo.relay.leased_to("0000000000000007"));
	// The page gone, its watch goes, and tyo is told at once rather than left to the lease's end.
	drop(events);
	until("rdu's page gone", || !rdu.relay.watched()).await;
	until("tyo released", || !tyo.relay.leased_to("0000000000000007")).await;
}

/// An event stream as a browser reads it: each `data:` line, as it arrives.
struct Events {
	body: Incoming,
	held: String,
}

impl Events {
	async fn open(address: SocketAddr, query: &str) -> (hyper::http::HeaderMap, Self) {
		let client: Client<HttpConnector, Empty<Bytes>> =
			Client::builder(TokioExecutor::new()).build(HttpConnector::new());
		let request = hyper::Request::get(format!("http://{address}/live{query}"))
			.header("accept", "text/event-stream")
			.body(Empty::new())
			.unwrap();
		let answer = client.request(request).await.unwrap();
		let headers = answer.headers().clone();
		(headers, Self { body: answer.into_body(), held: String::new() })
	}

	/// The next event's data, read as it arrives.
	async fn next(&mut self) -> serde_json::Value {
		loop {
			if let Some(end) = self.held.find("\n\n") {
				let block: String = self.held.drain(..end + 2).collect();
				if let Some(data) = block.lines().find_map(|line| line.strip_prefix("data: ")) {
					return serde_json::from_str(data).unwrap();
				}
				continue;
			}
			let frame =
				tokio::time::timeout(PATIENCE, self.body.frame()).await.expect("an event in time");
			if let Ok(data) = frame.unwrap().unwrap().into_data() {
				self.held.push_str(std::str::from_utf8(&data).unwrap());
			}
		}
	}
}

#[tokio::test]
async fn a_browser_that_asks_for_an_event_stream_is_sent_the_cluster_then_each_change() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:7").await;
	hold(&rdu, &tyo);
	hold(&tyo, &rdu);
	until("rdu holds tyo", || image(&rdu.relay, "0000000000000001").as_deref() == Some("geo:7"))
		.await;
	let (headers, mut events) = Events::open(rdu.address, "?deltas=true").await;
	assert_eq!(headers["content-type"], "text/event-stream");
	let first = events.next().await;
	assert_eq!((&first["type"], &first["node"]), (&"cluster".into(), &"0000000000000007".into()));
	let before = first["nodes"]["0000000000000001"]["version"].as_u64().unwrap();
	tyo.host.deploy("geo:8");
	let changed = async {
		loop {
			let next = events.next().await;
			let rows = &next["snapshot"]["apps"]["set"];
			if next["type"] == "delta"
				&& next["node"] == "0000000000000001"
				&& rows[0]["image"] == "geo:8"
			{
				assert_eq!(next["base"].as_u64(), Some(before));
				return;
			}
		}
	};
	tokio::time::timeout(PATIENCE, changed).await.expect("tyo's change in time");
	// A round's beats come on the same stream as one message, between the changes.
	let beats = async {
		loop {
			let next = events.next().await;
			assert_ne!(next["type"], "beat", "a page is sent a round's beats, never one a node");
			if next["type"] == "beats" {
				return next;
			}
		}
	};
	let beats = tokio::time::timeout(PATIENCE, beats).await.expect("a round's beats in time");
	assert!(beats["beats"].as_object().is_some_and(|beats| !beats.is_empty()));
}

/// The outcome the relay at `address` holds of `node`'s run `id`, as `/runs` answers it.
async fn outcome(address: SocketAddr, node: &str, id: i64) -> Option<String> {
	let client: Client<HttpConnector, Empty<Bytes>> =
		Client::builder(TokioExecutor::new()).build(HttpConnector::new());
	let answer: hyper::Response<Incoming> =
		client.get(format!("http://{address}/runs").parse().ok()?).await.ok()?;
	let body = answer.into_body().collect().await.ok()?.to_bytes();
	let runs: serde_json::Value = relay::host::opened(&body).ok()?;
	let run = runs["runs"].as_array()?.iter().find(|run| run["node"] == node && run["id"] == id)?;
	run["outcome"].as_str().map(str::to_owned)
}

#[tokio::test]
async fn a_run_reaches_the_other_relay_and_so_does_its_change() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:2").await;
	// Written before either meets the other: the reading back carries it, then the window.
	tyo.host.record(7, "running");
	hold(&rdu, &tyo);
	hold(&tyo, &rdu);
	let held = || async { outcome(rdu.address, "0000000000000001", 7).await };
	let waited = async {
		while held().await.as_deref() != Some("running") {
			tokio::time::sleep(ROUND).await;
		}
	};
	tokio::time::timeout(PATIENCE, waited).await.expect("rdu hears tyo's run");

	tyo.host.record(7, "succeeded");
	tyo.host.record(8, "running");
	let waited = async {
		while held().await.as_deref() != Some("succeeded")
			|| outcome(rdu.address, "0000000000000001", 8).await.is_none()
		{
			tokio::time::sleep(ROUND).await;
		}
	};
	tokio::time::timeout(PATIENCE, waited).await.expect("rdu hears tyo's run change");
	assert_eq!(outcome(tyo.address, "0000000000000001", 8).await.as_deref(), Some("running"));
}

#[tokio::test]
async fn a_relay_stopping_tells_its_neighbor_why_before_it_goes() {
	let rdu = start("0000000000000007", "geo:1").await;
	let tyo = start("0000000000000001", "geo:2").await;
	hold(&rdu, &tyo);
	hold(&tyo, &rdu);
	until("each holds the other", || image(&rdu.relay, "0000000000000001").is_some()).await;
	until("tyo speaks to rdu", || tyo.relay.speaking().contains("0000000000000007")).await;
	assert_eq!(rdu.relay.state().nodes["0000000000000001"].state, Presence::Live);
	let (mut browser, _) =
		tokio_tungstenite::connect_async(format!("ws://{}/live", rdu.address)).await.unwrap();

	tyo.host.upgrading();
	assert!(relay::leaving::leave(&tyo.relay, &*tyo.host).await, "rdu told in time");
	until("rdu shows tyo upgrading", || {
		rdu.relay.state().nodes["0000000000000001"].state == Presence::Upgrading
	})
	.await;
	let state = rdu.relay.state();
	let leaving = &state.nodes["0000000000000001"].held.as_ref().unwrap().snapshot["leaving"];
	assert_eq!(*leaving, serde_json::json!({ "reason": "upgrade", "within": 180 }));
	// A browser is sent it as a node's state, beside the snapshot that says why.
	loop {
		let next = text(&mut browser).await;
		if next["type"] == "node"
			&& next["node"] == "0000000000000001"
			&& next["state"]["state"] == "upgrading"
		{
			assert_eq!(next["state"]["snapshot"]["leaving"]["reason"], "upgrade");
			break;
		}
	}
}

#[tokio::test]
async fn a_relay_without_the_secret_is_refused() {
	let rdu = start("0000000000000007", "geo:1").await;
	let stranger = Peer { id: "0000000000000007".into(), address: rdu.address.to_string() };
	assert!(relay::mesh::dial(&stranger, "wrong").await.is_err());
	assert!(relay::mesh::dial(&stranger, SECRET).await.is_ok());
}

/// Both ends without Nagle's delay, so a pong is not held back for the ACK of what went before it.
#[tokio::test]
async fn a_socket_dialed_and_one_accepted_send_without_delay() {
	let rdu = start("0000000000000007", "geo:1").await;
	let peer = Peer { id: "0000000000000007".into(), address: rdu.address.to_string() };
	let dialed = relay::mesh::dial(&peer, SECRET).await.unwrap();
	let MaybeTlsStream::Plain(stream) = dialed.get_ref() else { panic!("a plain stream") };
	assert!(stream.nodelay().unwrap());
	// The kernel's round trip where it can be read, and none, for the pings to stand, elsewhere.
	assert_eq!(dialed.round_trip().is_some(), cfg!(target_os = "linux"));

	let mut listener =
		relay::mesh::accepting(tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap());
	let address = listener.local_addr().unwrap();
	let dialing = tokio::spawn(tokio::net::TcpStream::connect(address));
	let (accepted, _) = listener.accept().await;
	assert!(accepted.nodelay().unwrap());
	dialing.await.unwrap().unwrap();
}

async fn text<S>(socket: &mut S) -> serde_json::Value
where
	S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
	loop {
		let next = tokio::time::timeout(PATIENCE, socket.next()).await.expect("a message in time");
		if let Message::Text(text) = next.unwrap().unwrap() {
			return serde_json::from_str(text.as_str()).unwrap();
		}
	}
}
