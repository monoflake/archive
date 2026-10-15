//! The relays' mesh: a socket to every other node's relay. Both ends open with the versions they
//! hold and send each other what the other lacks; after that a relay sends its own node's snapshot
//! the moment it changes, a beat every round, and every other node's snapshot when a neighbor's
//! comparison shows it behind. The kernel's round trip on the socket it dialed, or else its pings
//! timed, is what its minutes carry to each neighbor. The runs and the minutes travel the same way,
//! each by their own versions, and a node's fine readings only to a neighbor holding a lease. See
//! spec/architecture/relay.md.

use crate::cluster::{Carried, Versions};
use crate::config::Peer;
use crate::delta::Changes;
use crate::relay::Relay;
use crate::runs::Batch;
use crate::socket::{Dial, Heard, Reader, Received, SocketError};
use axum::http::HeaderMap;
use axum::http::header::{AUTHORIZATION, InvalidHeaderValue};
use axum::serve::{Listener, ListenerExt};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;
use tokio::net::{TcpListener, TcpStream};
use tokio::time::Instant;
use tokio_tungstenite::tungstenite;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

mod answer;
mod converse;
mod outbox;
#[cfg(test)]
mod tests;

pub use converse::converse;

/// Where a relay accepts its neighbors.
pub const PATH: &str = "/mesh";

/// How often a relay tells a neighbor what it holds, and so the most a snapshot or a beat waits at
/// each hop past its own node, on a connection to one older than the sync.
pub const COMPARISON: Duration = Duration::from_secs(2);

/// How often a relay sends a synced neighbor every node's version, digest and newest beat: a round,
/// so its own beat is in every one.
pub const SYNC: Duration = crate::own::EVERY;

/// One node in a sync: its version, the digest of it in hex, and its newest beat.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Line(pub u64, pub String, pub u64);

/// Cloudflare and NATs close a socket idle for long; a ping keeps it from looking idle. A browser's
/// socket is pinged this often; a neighbor's, every `PING`.
pub const HEARTBEAT: Duration = Duration::from_secs(30);

/// How often a relay pings each neighbor and times the pong, and reads the kernel's round trip
/// where it can, the first as the socket opens.
/// Its own round is every `own::EVERY`, so a faster ping times a figure no round carries; a slower
/// one leaves the figure a round behind. See spec/architecture/relay.md, "The round trip to each
/// neighbor".
pub const PING: Duration = crate::own::EVERY;

/// How long the other end has to say who it is.
const INTRODUCTION: Duration = Duration::from_secs(10);

const FIRST_RETRY: Duration = Duration::from_secs(1);
const LAST_RETRY: Duration = Duration::from_secs(60);

/// What one relay sends another. `runs` is absent from a relay older than the runs, which is
/// never sent `Runs`, a kind it cannot read; so the contract's version stands. `minutes` is the
/// same of the minutes, `beats` of `Beat` and `watch` of `Watch` and `Readings`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum Message {
	/// Sent first by both ends: the contract's version, who it is, and what it holds. Every `node`
	/// in a message, and every key of `Versions` and `Sync`, is a node's id.
	Hello {
		version: u32,
		node: String,
		versions: Versions,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		runs: Option<Versions>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		minutes: Option<Versions>,
		/// Whether it hears a node by its beats, and so needs no new version every round.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		beats: Option<bool>,
		/// Whether it lends and reads fine readings.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		watch: Option<bool>,
		/// Whether it reads compressed frames; see `compression`.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		zstd: Option<bool>,
		/// The digest it syncs by, `delta::DIGEST`; a neighbor naming another, or none, is compared
		/// with as before.
		#[serde(default, skip_serializing_if = "Option::is_none")]
		sync: Option<String>,
	},
	/// What it holds, every comparison; answered with what it lacks. `beats` is each node's newest
	/// beat held, so a node heard only through a neighbor stays live.
	Versions {
		versions: Versions,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		runs: Option<Versions>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		minutes: Option<Versions>,
		#[serde(default, skip_serializing_if = "Option::is_none")]
		beats: Option<Versions>,
	},
	/// A node's snapshot, newer than what the other was last known to hold.
	Node { node: String, state: Carried },
	/// Some of a node's runs, which the other was last known to lack.
	Runs(Batch),
	/// Some of a node's minutes, which the other was last known to lack.
	Minutes(crate::history::Batch),
	/// That its node is alive, at its clock in milliseconds: every round, and nothing else.
	Beat { node: String, at: u64 },
	/// A page on the sender asks this node's fine readings, for `lease` seconds.
	Watch { lease: u64 },
	/// The node's fine readings of a round, to a neighbor holding a lease.
	Readings { node: String, sample: serde_json::Value },
	/// Every three seconds on a synced connection: each node held, its version, digest and newest
	/// beat; and the runs' and minutes' versions. Answered with what the other lacks, and a node
	/// whose digest differs at the same version, whole.
	Sync { nodes: BTreeMap<String, Line>, runs: Versions, minutes: Versions },
	/// What takes `node` from `base` to `version`, whose result reads `digest`.
	Delta {
		node: String,
		base: u64,
		version: u64,
		digest: String,
		#[serde(flatten)]
		changes: Changes,
	},
	/// A node asked for whole, by a relay that could not take a delta of it.
	Want { node: String },
}

/// What a neighbor's hello says of it.
struct Said {
	peer: String,
	versions: Versions,
	runs: Option<Versions>,
	minutes: Option<Versions>,
	beats: bool,
	watch: bool,
	zstd: bool,
	sync: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum MeshError {
	#[error(transparent)]
	Socket(#[from] SocketError),
	#[error("dialing: {0}")]
	Dial(#[from] tungstenite::Error),
	#[error("no answer within {0:?}")]
	Slow(Duration),
	#[error("a message this relay cannot read: {0}")]
	Malformed(#[from] serde_json::Error),
	#[error("a compressed frame this relay cannot read: {0}")]
	Corrupt(std::io::Error),
	#[error("it speaks version {0} of the mesh")]
	Version(u32),
	#[error("it opened with something other than hello")]
	Unintroduced,
	#[error("it says it is `{0}`")]
	Stranger(String),
	#[error("it says it is `{0}`, a node retired")]
	Retired(String),
	#[error("it closed the socket")]
	Closed,
}

/// Whether a request carries the mesh's secret, compared in time independent of where the first
/// difference is.
pub fn admitted(headers: &HeaderMap, secret: &str) -> bool {
	let given = headers
		.get(AUTHORIZATION)
		.and_then(|value| value.to_str().ok())
		.and_then(|value| value.strip_prefix("Bearer "))
		.unwrap_or_default();
	!secret.is_empty()
		&& given.len() == secret.len()
		&& given.bytes().zip(secret.bytes()).fold(0, |acc, (a, b)| acc | (a ^ b)) == 0
}

async fn introduced<R: Reader>(reader: &mut R) -> Result<Message, MeshError> {
	loop {
		match reader.receive().await? {
			Received::Text(text) => return Ok(serde_json::from_str(&text)?),
			Received::Closed => return Err(MeshError::Closed),
			// The hello is never compressed; anything before it is no introduction.
			Received::Binary(_) => return Err(MeshError::Unintroduced),
			Received::Pong(_) | Received::Other => {}
		}
	}
}

/// Opens a socket to `peer`'s relay, carrying the secret.
pub async fn dial(peer: &Peer, secret: &str) -> Result<Dial, MeshError> {
	let mut request = format!("ws://{}{PATH}", peer.address).into_client_request()?;
	let bearer = format!("Bearer {secret}")
		.parse()
		.map_err(|error: InvalidHeaderValue| tungstenite::Error::HttpFormat(error.into()))?;
	request.headers_mut().insert(AUTHORIZATION, bearer);
	// Without TCP_NODELAY a pong waits on the ACK of what went before it, a round trip more.
	let dialing = tokio_tungstenite::connect_async_with_config(request, None, true);
	let (socket, _) = tokio::time::timeout(INTRODUCTION, dialing)
		.await
		.map_err(|_| MeshError::Slow(INTRODUCTION))??;
	Ok(socket)
}

/// `listener`, its every connection without Nagle's delay, as `dial`'s are.
pub fn accepting(listener: TcpListener) -> impl Listener<Io = TcpStream, Addr = SocketAddr> {
	listener.tap_io(|stream: &mut TcpStream| {
		if let Err(error) = stream.set_nodelay(true) {
			eprintln!("relay: setting TCP_NODELAY on a connection: {error}");
		}
	})
}

/// Holds a socket to `peer` for as long as the relay runs, dialing again after each loss, waiting
/// twice as long each time it fails up to a minute. A neighbor down waits alone.
pub async fn keep(relay: Arc<Relay>, peer: Peer) {
	let mut wait = FIRST_RETRY;
	loop {
		match dial(&peer, relay.secret()).await {
			Ok(socket) => {
				eprintln!("relay: holding {}", peer.id);
				let opened = Instant::now();
				let ended = converse(relay.clone(), Some(&peer.id), Heard::new(socket)).await;
				match ended {
					Ok(()) => eprintln!("relay: {} closed", peer.id),
					Err(error) => eprintln!("relay: lost {}: {error}", peer.id),
				}
				if opened.elapsed() >= LAST_RETRY {
					wait = FIRST_RETRY;
				}
			}
			Err(error) => eprintln!("relay: dialing {} at {}: {error}", peer.id, peer.address),
		}
		tokio::time::sleep(wait).await;
		wait = (wait * 2).min(LAST_RETRY);
	}
}
