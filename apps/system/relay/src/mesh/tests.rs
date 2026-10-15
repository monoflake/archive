//! The mesh's tests: the fakes a conversation is fed, and the meeting.

mod frames;
mod pressure;
mod sync;

use super::converse::{round_trip, stamp};
use super::*;
use crate::cluster::{Carried, Versions};
use crate::compression::{Packer, Unpacker};
use crate::delta::{DIGEST, hex};
use crate::relay::{Relay, VERSION};
use crate::runs::Batch;
use crate::socket::{Dial, Frame, Halves, Heard, Reader, Received, SocketError, Writer};
use axum::http::HeaderMap;
use axum::http::HeaderValue;
use axum::http::header::AUTHORIZATION;
use bytes::Bytes;
use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::MaybeTlsStream;

fn relay(node: &str) -> Arc<Relay> {
	Relay::new(
		node.into(),
		"s3cret".into(),
		Default::default(),
		crate::runs::Store::memory().unwrap(),
	)
	.unwrap()
}

/// A socket whose other end is the test: what it is fed, it receives, and what it sends is kept.
/// Given an `echo`, it answers its own pings, as the other end's socket does beneath.
struct Fake {
	incoming: mpsc::UnboundedReceiver<Received>,
	sent: Arc<Mutex<Vec<Message>>>,
	echo: Option<mpsc::UnboundedSender<Received>>,
}

/// What a test feeds a socket, as its reading half.
struct Fed(mpsc::UnboundedReceiver<Received>);

impl Reader for Fed {
	async fn receive(&mut self) -> Result<Received, SocketError> {
		Ok(self.0.recv().await.unwrap_or(Received::Closed))
	}
}

/// Where a fake keeps what it is sent, as its writing half, a compressed frame unpacked.
struct Kept {
	sent: Arc<Mutex<Vec<Message>>>,
	echo: Option<mpsc::UnboundedSender<Received>>,
	unpacker: Option<Unpacker>,
}

impl Writer for Kept {
	async fn send(&mut self, frame: Frame) -> Result<(), SocketError> {
		match frame {
			Frame::Text(text) => self.sent.lock().unwrap().push(serde_json::from_str(&text).unwrap()),
			Frame::Binary(packed) => {
				let unpacker = self.unpacker.get_or_insert_with(|| Unpacker::new().unwrap());
				let text = unpacker.unpack(&packed).unwrap();
				self.sent.lock().unwrap().push(serde_json::from_slice(&text).unwrap());
			}
			Frame::Ping(payload) => {
				if let Some(echo) = &self.echo {
					let _ = echo.send(Received::Pong(payload));
				}
			}
		}
		Ok(())
	}
}

impl Halves for Fake {
	type Reader = Fed;
	type Writer = Kept;

	fn halves(self) -> (Fed, Kept) {
		(Fed(self.incoming), Kept { sent: self.sent, echo: self.echo, unpacker: None })
	}
}

#[tokio::test]
async fn meeting_sends_hello_and_then_exactly_what_the_other_lacks() {
	let relay = relay("d62c82b23a858876");
	let snapshot = Arc::new(serde_json::json!({}));
	for (node, version) in [
		("d62c82b23a858876", 9),
		("d132b0f553e3dcc3", 4),
		("e38f403f033d87f7", 7),
		("a0a24bbaa0c0beba", 2),
	] {
		relay.take(node, Carried { version, snapshot: snapshot.clone() }, None);
	}
	let (feed, incoming) = mpsc::unbounded_channel();
	let sent = Arc::new(Mutex::new(Vec::new()));
	let theirs = Versions::from([
		("d132b0f553e3dcc3".into(), 6),
		("e38f403f033d87f7".into(), 7),
		("a0a24bbaa0c0beba".into(), 1),
	]);
	let hello = Message::Hello {
		version: VERSION,
		node: "d132b0f553e3dcc3".into(),
		versions: theirs,
		runs: None,
		minutes: None,
		beats: None,
		watch: None,
		zstd: None,
		sync: None,
	};
	feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
	drop(feed);

	let fake = Heard::new(Fake { incoming, sent: sent.clone(), echo: None });
	converse(relay.clone(), Some("d132b0f553e3dcc3"), fake).await.unwrap();
	let sent = sent.lock().unwrap();
	assert!(matches!(&sent[0], Message::Hello { node, versions, .. }
		if node == "d62c82b23a858876" && *versions == relay.versions()));
	// Not tyo's own, however old it is here, and not buf, which it holds as new.
	let nodes: Vec<(&str, u64)> = sent[1..]
		.iter()
		.map(|message| match message {
			Message::Node { node, state } => (node.as_str(), state.version),
			other => panic!("sent {other:?}"),
		})
		.collect();
	assert_eq!(nodes, [("a0a24bbaa0c0beba", 2), ("d62c82b23a858876", 9)]);
}

/// A relay of version 1 names each node by its name, so its nodes would be held twice over.
#[tokio::test]
async fn a_neighbor_speaking_names_is_refused() {
	let relay = relay("d62c82b23a858876");
	let (feed, incoming) = mpsc::unbounded_channel();
	let hello = Message::Hello {
		version: 1,
		node: "tyo".into(),
		versions: Versions::from([("tyo".into(), 4)]),
		runs: None,
		minutes: None,
		beats: None,
		watch: None,
		zstd: None,
		sync: None,
	};
	feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
	drop(feed);
	let fake = Heard::new(Fake { incoming, sent: Arc::new(Mutex::new(Vec::new())), echo: None });
	let ended = converse(relay.clone(), Some("d132b0f553e3dcc3"), fake).await;
	assert!(matches!(ended, Err(MeshError::Version(1))), "{ended:?}");
	assert!(!relay.versions().contains_key("tyo"));
}

/// tyo's hello, holding `runs` of the runs or older than them.
async fn met(relay: &Arc<Relay>, runs: Option<Versions>) -> Vec<Message> {
	let (feed, incoming) = mpsc::unbounded_channel();
	let sent = Arc::new(Mutex::new(Vec::new()));
	let hello = Message::Hello {
		version: VERSION,
		node: "d132b0f553e3dcc3".into(),
		versions: Versions::new(),
		runs,
		minutes: None,
		beats: None,
		watch: None,
		zstd: None,
		sync: None,
	};
	feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
	drop(feed);
	let fake = Heard::new(Fake { incoming, sent: sent.clone(), echo: None });
	converse(relay.clone(), Some("d132b0f553e3dcc3"), fake).await.unwrap();
	sent.lock().unwrap().clone()
}

#[tokio::test]
async fn a_neighbor_is_sent_the_runs_it_lacks_and_one_older_than_runs_none() {
	let relay = relay("d62c82b23a858876");
	let row = |id: i64, version: u64| crate::runs::Row {
		version,
		event: crate::host::Event {
			id,
			app: "geo".into(),
			action: "deploy".into(),
			source: crate::host::Source { kind: "panel".into(), run: None, commit: None },
			image: None,
			outcome: "succeeded".into(),
			stage: None,
			detail: None,
			started_at: jiff::Timestamp::now().to_string(),
			finished_at: None,
		},
	};
	for (node, version) in [("e38f403f033d87f7", 7), ("d132b0f553e3dcc3", 4)] {
		relay.take_runs(Batch { node: node.into(), above: 0, version, rows: vec![row(1, version)] });
	}

	let runs: Vec<(String, u64, usize)> =
		met(&relay, Some(Versions::from([("e38f403f033d87f7".into(), 3)])))
			.await
			.into_iter()
			.filter_map(|message| match message {
				Message::Runs(batch) => Some((batch.node, batch.above, batch.rows.len())),
				_ => None,
			})
			.collect();
	// buf above what tyo holds of it, and never tyo's own.
	assert_eq!(runs, [("e38f403f033d87f7".into(), 3, 1)]);

	let older = met(&relay, None).await;
	assert!(matches!(&older[0], Message::Hello { runs: Some(_), .. }));
	assert!(!older.iter().any(|message| matches!(message, Message::Runs(_))));
}

/// tyo's hello, holding `minutes` of the minutes or older than them.
async fn met_minutes(relay: &Arc<Relay>, minutes: Option<Versions>) -> Vec<Message> {
	let (feed, incoming) = mpsc::unbounded_channel();
	let sent = Arc::new(Mutex::new(Vec::new()));
	let hello = Message::Hello {
		version: VERSION,
		node: "d132b0f553e3dcc3".into(),
		versions: Versions::new(),
		runs: None,
		minutes,
		beats: None,
		watch: None,
		zstd: None,
		sync: None,
	};
	feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
	drop(feed);
	let fake = Heard::new(Fake { incoming, sent: sent.clone(), echo: None });
	converse(relay.clone(), Some("d132b0f553e3dcc3"), fake).await.unwrap();
	sent.lock().unwrap().clone()
}

#[tokio::test]
async fn a_neighbor_is_sent_the_minutes_it_lacks_and_one_older_than_minutes_none() {
	let relay = relay("d62c82b23a858876");
	let minute = |version: u64, at: i64| crate::history::Row {
		version,
		minute: serde_json::from_value(serde_json::json!({ "at": at, "beats": 20 })).unwrap(),
	};
	for (node, version) in [("e38f403f033d87f7", 7), ("d132b0f553e3dcc3", 4)] {
		let rows = vec![minute(version, 1_791_499_980_000)];
		relay.take_minutes(crate::history::Batch { node: node.into(), above: 0, version, rows });
	}
	let sent = met_minutes(&relay, Some(Versions::from([("e38f403f033d87f7".into(), 3)]))).await;
	assert!(matches!(&sent[0], Message::Hello { minutes: Some(minutes), .. }
		if minutes.get("e38f403f033d87f7") == Some(&7)));
	let minutes: Vec<(String, u64, usize)> = sent
		.into_iter()
		.filter_map(|message| match message {
			Message::Minutes(batch) => Some((batch.node, batch.above, batch.rows.len())),
			_ => None,
		})
		.collect();
	// buf above what tyo holds of it, and never tyo's own.
	assert_eq!(minutes, [("e38f403f033d87f7".into(), 3, 1)]);
	let older = met_minutes(&relay, None).await;
	assert!(!older.iter().any(|message| matches!(message, Message::Minutes(_))));
	// A minute not on a minute's start is no minute, and is not taken.
	let odd = vec![minute(9, 1_791_499_980_001)];
	relay.take_minutes(crate::history::Batch {
		node: "a0a24bbaa0c0beba".into(),
		above: 0,
		version: 9,
		rows: odd,
	});
	let lacking = relay.lacking_minutes(&Versions::new(), "e38f403f033d87f7").await.unwrap();
	assert!(lacking.iter().find(|batch| batch.node == "a0a24bbaa0c0beba").unwrap().rows.is_empty());
}

#[tokio::test]
async fn a_neighbor_that_is_not_the_one_dialed_is_refused() {
	let relay = relay("d62c82b23a858876");
	for (said, expected) in
		[("e38f403f033d87f7", Some("d132b0f553e3dcc3")), ("d62c82b23a858876", None)]
	{
		let (feed, incoming) = mpsc::unbounded_channel();
		let hello = Message::Hello {
			version: VERSION,
			node: said.into(),
			versions: Versions::new(),
			runs: None,
			minutes: None,
			beats: None,
			watch: None,
			zstd: None,
			sync: None,
		};
		feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
		let fake = Heard::new(Fake { incoming, sent: Arc::default(), echo: None });
		let refused = converse(relay.clone(), expected, fake).await;
		assert!(matches!(refused, Err(MeshError::Stranger(named)) if named == said));
	}
}

#[tokio::test]
async fn a_neighbor_held_as_retired_is_refused() {
	let relay = relay("d62c82b23a858876");
	relay.set_retired(std::collections::BTreeSet::from(["e38f403f033d87f7".to_owned()]));
	for (said, refused) in [("e38f403f033d87f7", true), ("d132b0f553e3dcc3", false)] {
		let (feed, incoming) = mpsc::unbounded_channel();
		let hello = Message::Hello {
			version: VERSION,
			node: said.into(),
			versions: Versions::new(),
			runs: None,
			minutes: None,
			beats: None,
			watch: None,
			zstd: None,
			sync: None,
		};
		feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
		feed.send(Received::Closed).unwrap();
		let fake = Heard::new(Fake { incoming, sent: Arc::default(), echo: None });
		let answered = converse(relay.clone(), None, fake).await;
		let named = matches!(&answered, Err(MeshError::Retired(named)) if named == said);
		assert_eq!(named, refused, "{said}: {answered:?}");
	}
}

#[tokio::test]
async fn a_pong_times_the_round_trip_to_the_neighbor_that_sent_it() {
	let relay = relay("d62c82b23a858876");
	let (feed, incoming) = mpsc::unbounded_channel();
	let hello = Message::Hello {
		version: VERSION,
		node: "d132b0f553e3dcc3".into(),
		versions: Versions::new(),
		runs: None,
		minutes: None,
		beats: None,
		watch: None,
		zstd: None,
		sync: None,
	};
	feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
	let fake = Heard::new(Fake { incoming, sent: Arc::default(), echo: Some(feed) });
	let conversing = tokio::spawn(converse(relay.clone(), Some("d132b0f553e3dcc3"), fake));
	let timed = async {
		while relay.round_trips().is_empty() {
			tokio::time::sleep(Duration::from_millis(5)).await;
		}
	};
	tokio::time::timeout(Duration::from_secs(5), timed).await.expect("the first ping, timed");
	conversing.abort();
	let round_trips = relay.round_trips();
	assert_eq!(round_trips.keys().collect::<Vec<_>>(), ["d132b0f553e3dcc3"]);
	assert!((0.0..1000.0).contains(&round_trips["d132b0f553e3dcc3"]));
}

#[test]
fn a_pong_carries_back_when_its_ping_was_sent() {
	let origin = Instant::now();
	let sent = origin + Duration::from_millis(40);
	let payload = stamp(origin, sent);
	let answered = sent + Duration::from_micros(151_250);
	assert_eq!(round_trip(origin, &payload, answered), Some(Duration::from_micros(151_250)));
	// One stamped later than now, and one this socket never sent, time nothing.
	assert_eq!(round_trip(origin, &payload, origin), None);
	assert_eq!(round_trip(origin, b"", answered), None);
	assert_eq!(round_trip(origin, b"four", answered), None);
}

#[test]
fn the_secret_admits_and_nothing_else_does() {
	let with = |value: &str| {
		let mut headers = HeaderMap::new();
		headers.insert(AUTHORIZATION, HeaderValue::from_str(value).unwrap());
		headers
	};
	assert!(admitted(&with("Bearer s3cret"), "s3cret"));
	let wrongs =
		["Bearer s3cre", "Bearer s3cret!", "Bearer S3cret", "s3cret", "Basic s3cret", "Bearer "];
	for wrong in wrongs {
		assert!(!admitted(&with(wrong), "s3cret"), "{wrong}");
	}
	assert!(!admitted(&HeaderMap::new(), "s3cret"));
}

#[test]
fn the_messages_are_tagged_by_type() {
	let versions = Versions::from([("d132b0f553e3dcc3".into(), 7)]);
	let hello = Message::Hello {
		version: 1,
		node: "d62c82b23a858876".into(),
		versions,
		runs: None,
		minutes: None,
		beats: None,
		watch: None,
		zstd: None,
		sync: None,
	};
	assert_eq!(
		serde_json::to_value(&hello).unwrap(),
		serde_json::json!({ "type": "hello", "version": 1, "node": "d62c82b23a858876", "versions": { "d132b0f553e3dcc3": 7 } })
	);
	let node: Message = serde_json::from_str(
		r#"{ "type": "node", "node": "d132b0f553e3dcc3", "state": { "version": 8, "snapshot": { "apps": [] } } }"#,
	)
	.unwrap();
	assert!(
		matches!(node, Message::Node { node, state } if node == "d132b0f553e3dcc3" && state.version == 8)
	);

	// A relay older than the runs says nothing of them, and is read as such.
	let older: Message =
		serde_json::from_str(r#"{ "type": "versions", "versions": { "d132b0f553e3dcc3": 7 } }"#)
			.unwrap();
	assert!(matches!(older, Message::Versions { runs: None, minutes: None, .. }));
	let runs: Message = serde_json::from_str(
		r#"{ "type": "runs", "node": "d132b0f553e3dcc3", "above": 3, "version": 9, "rows": [{ "version": 9,
			"id": 1, "app": "geo", "action": "deploy", "source": { "kind": "panel" },
			"outcome": "running", "started_at": "2026-10-09T12:00:00Z" }] }"#,
	)
	.unwrap();
	assert!(matches!(runs, Message::Runs(Batch { node, above: 3, version: 9, rows })
		if node == "d132b0f553e3dcc3" && rows[0].event.id == 1 && rows[0].version == 9));
}
