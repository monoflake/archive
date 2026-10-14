//! A browser's socket: every node at once, then each newer snapshot as this relay takes it, each
//! state time alone changed, each node's beat, and the fine readings of a node a page here watches.
//! Nothing a browser sends is acted on. See spec/architecture/console.md, "Live, through the
//! nearest node".

use crate::delta::{Changes, diff};
use crate::mesh::HEARTBEAT;
use crate::presence::Entry;
use crate::relay::{Change, Pulse, Relay, State};
use crate::socket::{Heard, Received, Socket, SocketError};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::sync::Arc;
use tokio::sync::broadcast::error::RecvError;
use tokio::time::Instant;
use url::Url;

/// Whether a socket may open from `origin`: a page on the platform's `.app` or a name under it, or
/// no page at all -- a client that is not a browser sends no `Origin`. Anything else is a page
/// elsewhere borrowing the reader's Access cookie.
pub fn admitted(origin: Option<&str>) -> bool {
	let Some(origin) = origin else {
		return true;
	};
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
	/// That a node was heard, between its entries: its new `heard_at`.
	Beat {
		node: String,
		heard_at: Timestamp,
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
	let mut changes = relay.subscribe_changes();
	let mut pulses = relay.subscribe_pulses();
	let state = relay.state();
	// What this socket holds of each node, for the next delta to be taken from.
	let mut sent = if deltas { entries(&state) } else { BTreeMap::new() };
	send(&mut socket, &Live::Cluster(state)).await?;
	let mut heartbeat = tokio::time::interval_at(Instant::now() + HEARTBEAT, HEARTBEAT);
	loop {
		tokio::select! {
			received = socket.receive() => {
				if received? == Received::Closed {
					return Ok(());
				}
			}
			change = changes.recv() => match change {
				Ok(Change { node, entry }) => {
					let after = deltas.then(|| serde_json::to_value(&entry).ok()).flatten();
					match after {
						Some(after) => {
							let delta = match sent.get(&node) {
								Some(before) => entry_delta(&node, before, &after),
								None => Some(Live::Node { node: node.clone(), state: entry }),
							};
							sent.insert(node, after);
							if let Some(delta) = delta {
								send(&mut socket, &delta).await?;
							}
						}
						None => send(&mut socket, &Live::Node { node, state: entry }).await?,
					}
				}
				Err(RecvError::Lagged(_)) => {
					let state = relay.state();
					if deltas {
						sent = entries(&state);
					}
					send(&mut socket, &Live::Cluster(state)).await?;
				}
				Err(RecvError::Closed) => return Ok(()),
			},
			pulse = pulses.recv() => match pulse {
				Ok(Pulse::Beat { node, heard_at }) => {
					send(&mut socket, &Live::Beat { node, heard_at }).await?;
				}
				Ok(Pulse::Readings { node, sample }) => {
					send(&mut socket, &Live::Readings { node, sample }).await?;
				}
				// The next beat and the next readings say the same, newer.
				Err(RecvError::Lagged(_)) => {}
				Err(RecvError::Closed) => return Ok(()),
			},
			_ = heartbeat.tick() => {
				socket.ping(bytes::Bytes::new()).await?;
			}
		}
	}
}

async fn send<S: Socket>(socket: &mut S, live: &Live) -> Result<(), SocketError> {
	socket.send_text(serde_json::to_string(live).expect("a message is a tree of strings")).await
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
		let delta = serde_json::to_value(entry_delta("tyo", &before, &after).unwrap()).unwrap();
		assert_eq!(
			delta,
			serde_json::json!({ "type": "delta", "node": "tyo", "base": 10,
				"set": { "heard_at": "b", "runs": 4, "version": 20 },
				"snapshot": { "apps": { "set": [{ "name": "geo", "image": "geo:2" }] } } })
		);
		assert!(entry_delta("tyo", &after, &after).is_none());
		// A peer heard at last: its snapshot whole, from an entry that held no version.
		let unheard = serde_json::json!({ "state": "waiting" });
		let heard = serde_json::to_value(entry_delta("tyo", &unheard, &after).unwrap()).unwrap();
		assert!(heard.get("base").is_none() && heard["set"]["snapshot"]["apps"].is_array());
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
	fn a_client_that_is_no_browser_is_admitted() {
		assert!(admitted(None));
	}
}
