//! A browser's socket: every node at once, then each newer snapshot as this relay takes it, each
//! state time alone changed, a round's beats, and the fine readings of a node a page here watches.
//! Nothing a browser sends is acted on. See spec/architecture/console.md, "Live, through the
//! nearest node".

use crate::delta::{Changes, diff};
use crate::mesh::HEARTBEAT;
use crate::own::EVERY;
use crate::presence::Entry;
use crate::relay::{Change, Pulse, Relay, State};
use crate::socket::{Heard, Received, Socket, SocketError};
use axum::response::sse::{Event, KeepAlive, Sse};
use futures_util::{Stream, StreamExt};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::broadcast;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::{Instant, MissedTickBehavior};
use url::Url;

/// Whether a socket may open from `origin`: the console, a page on the platform's `.app` or a
/// name under it, or no page at all -- a client that is not a browser sends no `Origin`. Anything
/// else is a page elsewhere borrowing the reader's Access cookie.
pub fn admitted(origin: Option<&str>) -> bool {
	let Some(origin) = origin else {
		return true;
	};
	if origin == monoflake::INTERNAL_CONSOLE {
		return true;
	}
	// The console on `.app` until it is gone -- RFC o7eir.
	let (Ok(origin), Ok(app)) = (Url::parse(origin), Url::parse(monoflake::INTERNAL_APP)) else {
		return false;
	};
	let (Some(host), Some(suffix)) = (origin.host_str(), app.host_str()) else {
		return false;
	};
	let under =
		host.strip_suffix(suffix).is_some_and(|label| label.is_empty() || label.ends_with('.'));
	origin.scheme() == app.scheme() && origin.port() == app.port() && under
}

/// What a browser is sent. A browser keeps, per node, the highest version it has been sent.
#[derive(Debug, Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Live {
	/// First, and again whenever this socket fell too far behind to say what it missed.
	Cluster(State),
	Node {
		node: String,
		state: Entry,
	},
	/// The nodes heard in a round, between their entries: each one's new `heard_at`, gathered so a
	/// page is sent one message a round rather than one a node.
	Beats {
		beats: BTreeMap<String, Timestamp>,
	},
	/// A watched node's fine readings of a round, the meter's `sample`.
	Readings {
		node: String,
		sample: serde_json::Value,
	},
	/// To a socket that asked for them, what changed of a node's entry since the one it was last
	/// sent: its own keys, and its snapshot's as the mesh's delta says them, apps by name. `base` is
	/// the version that entry held; a browser holding another opens the socket again.
	Delta {
		node: String,
		#[serde(skip_serializing_if = "Option::is_none")]
		base: Option<u64>,
		#[serde(skip_serializing_if = "Map::is_empty")]
		set: Map<String, Value>,
		#[serde(skip_serializing_if = "Vec::is_empty")]
		unset: Vec<String>,
		#[serde(skip_serializing_if = "Option::is_none")]
		snapshot: Option<Changes>,
	},
}

const SNAPSHOT: &str = "snapshot";

/// What takes the entry `before` to `after`, or none where nothing did.
fn entry_delta(node: &str, before: &Value, after: &Value) -> Option<Live> {
	let strip = |entry: &Value| {
		let mut entry = entry.as_object().cloned().unwrap_or_default();
		let snapshot = entry.remove(SNAPSHOT);
		(Value::Object(entry), snapshot)
	};
	let ((before_rest, before_snapshot), (after_rest, after_snapshot)) =
		(strip(before), strip(after));
	let mut changes = diff(&before_rest, &after_rest);
	let snapshot = match (before_snapshot, after_snapshot) {
		(Some(before), Some(after)) => Some(diff(&before, &after)).filter(|one| !one.is_empty()),
		(None, Some(after)) => {
			changes.set.insert(SNAPSHOT.to_owned(), after);
			None
		}
		(Some(_), None) => {
			changes.unset.push(SNAPSHOT.to_owned());
			None
		}
		(None, None) => None,
	};
	if changes.is_empty() && snapshot.is_none() {
		return None;
	}
	let base = before.get("version").and_then(Value::as_u64);
	let (set, unset) = (changes.set, changes.unset);
	Some(Live::Delta { node: node.to_owned(), base, set, unset, snapshot })
}

/// Each node's entry as a socket was last sent it, from the whole cluster.
fn entries(state: &State) -> BTreeMap<String, Value> {
	let entries = state.nodes.iter();
	entries
		.filter_map(|(node, entry)| Some((node.clone(), serde_json::to_value(entry).ok()?)))
		.collect()
}

/// How often a browser's event stream says something when nothing else went out, so nothing on the
/// way takes it for idle.
pub const PING: Duration = Duration::from_secs(15);

/// What a browser is sent, in order, whichever way it reads: the whole cluster first, then each
/// node's changes -- its entry whole, or what changed of it to a reader that asked for `deltas` --
/// a round's beats, and the fine readings of a node a page here watches.
pub struct Feed {
	relay: Arc<Relay>,
	changes: broadcast::Receiver<Change>,
	pulses: broadcast::Receiver<Pulse>,
	deltas: bool,
	/// What this reader holds of each node, for the next delta to be taken from.
	sent: BTreeMap<String, Value>,
	/// The beats heard since the round's last went out, the newest of each node.
	beats: BTreeMap<String, Timestamp>,
	round: tokio::time::Interval,
}

impl Feed {
	/// A feed and the whole cluster it opens with, subscribed before the cluster is read so nothing
	/// taken meanwhile is missed.
	pub fn open(relay: Arc<Relay>, deltas: bool) -> (Self, Live) {
		Self::rounded(relay, deltas, EVERY)
	}

	/// The same, its beats gathered over `every` rather than the relay's round.
	fn rounded(relay: Arc<Relay>, deltas: bool, every: Duration) -> (Self, Live) {
		let (changes, pulses) = (relay.subscribe_changes(), relay.subscribe_pulses());
		let state = relay.state();
		let sent = if deltas { entries(&state) } else { BTreeMap::new() };
		let mut round = tokio::time::interval_at(Instant::now() + every, every);
		round.set_missed_tick_behavior(MissedTickBehavior::Delay);
		let beats = BTreeMap::new();
		(Self { relay, changes, pulses, deltas, sent, beats, round }, Live::Cluster(state))
	}

	/// The next message, or none once the relay stops. Cancel-safe, so it can wait in a `select!`.
	pub async fn next(&mut self) -> Option<Live> {
		loop {
			tokio::select! {
				change = self.changes.recv() => match change {
					Ok(Change { node, entry }) => {
						if let Some(live) = self.changed(node, entry) {
							return Some(live);
						}
					}
					Err(RecvError::Lagged(_)) => {
						let state = self.relay.state();
						if self.deltas {
							self.sent = entries(&state);
						}
						return Some(Live::Cluster(state));
					}
					Err(RecvError::Closed) => return None,
				},
				pulse = self.pulses.recv() => match pulse {
					Ok(Pulse::Beat { node, heard_at }) => {
						let held = self.beats.entry(node).or_insert(heard_at);
						*held = (*held).max(heard_at);
					}
					Ok(Pulse::Readings { node, sample }) => return Some(Live::Readings { node, sample }),
					// The next beat and the next readings say the same, newer.
					Err(RecvError::Lagged(_)) => {}
					Err(RecvError::Closed) => return None,
				},
				_ = self.round.tick() => {
					if !self.beats.is_empty() {
						return Some(Live::Beats { beats: std::mem::take(&mut self.beats) });
					}
				}
			}
		}
	}

	/// A node's new entry as this reader is sent it, or none where nothing in it changed.
	fn changed(&mut self, node: String, entry: Entry) -> Option<Live> {
		let after = self.deltas.then(|| serde_json::to_value(&entry).ok()).flatten();
		let Some(after) = after else {
			return Some(Live::Node { node, state: entry });
		};
		let live = match self.sent.get(&node) {
			Some(before) => entry_delta(&node, before, &after),
			None => Some(Live::Node { node: node.clone(), state: entry }),
		};
		self.sent.insert(node, after);
		live
	}
}

/// A browser's event stream: what a `Feed` says, a `data:` line each, and `: ping` every `PING`
/// when nothing else went out. It ends when the browser goes; a new one starts whole.
pub fn stream(
	relay: Arc<Relay>,
	deltas: bool,
) -> Sse<impl Stream<Item = Result<Event, Infallible>>> {
	let (feed, cluster) = Feed::open(relay, deltas);
	let event = |live: Live| Ok(Event::default().data(text(&live)));
	let rest = futures_util::stream::unfold(feed, move |mut feed| async move {
		let live = feed.next().await?;
		Some((event(live), feed))
	});
	let events = futures_util::stream::once(async move { event(cluster) }).chain(rest);
	Sse::new(events).keep_alive(KeepAlive::new().interval(PING).text("ping"))
}

/// Until the browser closes or falls silent; a browser gone quiet is no failure. A socket that
/// asked for `deltas` is sent each node's changes rather than its entry whole, after the cluster.
pub async fn watch<S: Socket>(
	relay: Arc<Relay>,
	socket: Heard<S>,
	deltas: bool,
) -> Result<(), SocketError> {
	match watching(relay, socket, deltas).await {
		Err(error) if error.silent() => Ok(()),
		ended => ended,
	}
}

async fn watching<S: Socket>(
	relay: Arc<Relay>,
	mut socket: Heard<S>,
	deltas: bool,
) -> Result<(), SocketError> {
	let (mut feed, cluster) = Feed::open(relay, deltas);
	send(&mut socket, &cluster).await?;
	let mut heartbeat = tokio::time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
	loop {
		tokio::select! {
			received = socket.receive() => {
				if received? == Received::Closed {
					return Ok(());
				}
			}
			live = feed.next() => match live {
				Some(live) => send(&mut socket, &live).await?,
				None => return Ok(()),
			},
			_ = heartbeat.tick() => {
				socket.ping(bytes::Bytes::new()).await?;
			}
		}
	}
}

fn text(live: &Live) -> String {
	serde_json::to_string(live).expect("a message is a tree of strings")
}

async fn send<S: Socket>(socket: &mut S, live: &Live) -> Result<(), SocketError> {
	socket.send_text(text(live)).await
}

#[cfg(test)]
mod tests {
	use super::*;

	fn named(host: &str) -> String {
		let app = Url::parse(monoflake::INTERNAL_APP).unwrap();
		format!("{}://{host}", app.scheme())
	}

	#[test]
	fn an_entrys_delta_is_its_keys_and_its_snapshots_apps_by_name_from_the_version_it_held() {
		let before = serde_json::json!({ "version": 10, "state": "live", "heard_at": "a",
			"snapshot": { "apps": [{ "name": "geo", "image": "geo:1" }, { "name": "apt", "image": "apt:1" }] } });
		let after = serde_json::json!({ "version": 20, "state": "live", "heard_at": "b",
			"snapshot": { "apps": [{ "name": "geo", "image": "geo:2" }, { "name": "apt", "image": "apt:1" }] },
			"runs": 4 });
		let delta =
			serde_json::to_value(entry_delta("d132b0f553e3dcc3", &before, &after).unwrap()).unwrap();
		assert_eq!(
			delta,
			serde_json::json!({ "type": "delta", "node": "d132b0f553e3dcc3", "base": 10,
				"set": { "heard_at": "b", "runs": 4, "version": 20 },
				"snapshot": { "apps": { "set": [{ "name": "geo", "image": "geo:2" }] } } })
		);
		assert!(entry_delta("d132b0f553e3dcc3", &after, &after).is_none());
		// A peer heard at last: its snapshot whole, from an entry that held no version.
		let unheard = serde_json::json!({ "state": "waiting" });
		let heard =
			serde_json::to_value(entry_delta("d132b0f553e3dcc3", &unheard, &after).unwrap()).unwrap();
		assert!(heard.get("base").is_none() && heard["set"]["snapshot"]["apps"].is_array());
	}

	#[tokio::test]
	async fn a_rounds_beats_go_to_a_page_as_one_message_the_newest_of_each_node() {
		use crate::cluster::Carried;
		let relay = Relay::new(
			"d62c82b23a858876".into(),
			"s3cret".into(),
			std::collections::BTreeSet::new(),
			crate::runs::Store::memory().unwrap(),
		)
		.unwrap();
		let now = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
		for node in ["d132b0f553e3dcc3", "e38f403f033d87f7"] {
			let carried = Carried { version: now - 1_000, snapshot: Arc::new(serde_json::json!({})) };
			relay.take(node, carried, None);
		}
		let (mut feed, _) = Feed::rounded(relay.clone(), true, Duration::from_millis(50));
		relay.beaten("d132b0f553e3dcc3", now, Some("e38f403f033d87f7"));
		relay.beaten("e38f403f033d87f7", now + 10, Some("e38f403f033d87f7"));
		relay.beaten("d132b0f553e3dcc3", now + 20, Some("e38f403f033d87f7"));
		let beats = loop {
			let next = tokio::time::timeout(Duration::from_secs(5), feed.next()).await.unwrap();
			if let Some(Live::Beats { beats }) = next {
				break beats;
			}
		};
		// Heard as this relay's clock says: tyo's second beat, after buf's, is the one it keeps.
		assert_eq!(beats.keys().collect::<Vec<_>>(), ["d132b0f553e3dcc3", "e38f403f033d87f7"]);
		assert!(beats["d132b0f553e3dcc3"] >= beats["e38f403f033d87f7"]);
		let text = text(&Live::Beats { beats });
		assert!(text.starts_with(r#"{"type":"beats","beats":{"d132b0f553e3dcc3":""#), "{text}");
		// A round nobody was heard in sends nothing.
		let quiet = tokio::time::timeout(Duration::from_millis(200), feed.next()).await;
		assert!(quiet.is_err());
	}

	#[test]
	fn a_page_on_the_app_or_under_it_is_admitted() {
		let suffix = Url::parse(monoflake::INTERNAL_APP).unwrap().host_str().unwrap().to_owned();
		assert!(admitted(Some(monoflake::INTERNAL_APP)));
		assert!(admitted(Some(&named(&format!("console.{suffix}")))));
		assert!(admitted(Some(&named(&format!("relay.{suffix}")))));
	}

	#[test]
	fn a_page_elsewhere_is_refused() {
		let suffix = Url::parse(monoflake::INTERNAL_APP).unwrap().host_str().unwrap().to_owned();
		for host in ["evil.test".to_owned(), format!("{suffix}.evil.test"), format!("evil{suffix}")] {
			assert!(!admitted(Some(&named(&host))), "{host}");
		}
		// The right name, by plain HTTP or on another port; and an opaque origin.
		assert!(!admitted(Some(&format!("http://{suffix}"))));
		assert!(!admitted(Some(&format!("{}:8443", monoflake::INTERNAL_APP))));
		assert!(!admitted(Some("null")));
	}

	#[test]
	fn the_console_is_admitted_by_its_exact_origin() {
		let console = Url::parse(monoflake::INTERNAL_CONSOLE).unwrap();
		let host = console.host_str().unwrap().to_owned();
		assert!(admitted(Some(monoflake::INTERNAL_CONSOLE)));
		for origin in [
			named(&format!("{host}.evil.test")),
			named(&format!("x{host}")),
			named(&format!("relay.{host}")),
			format!("http://{host}"),
			format!("{}:8443", monoflake::INTERNAL_CONSOLE),
		] {
			assert!(!admitted(Some(&origin)), "{origin}");
		}
	}

	#[test]
	fn a_client_that_is_no_browser_is_admitted() {
		assert!(admitted(None));
	}
}
