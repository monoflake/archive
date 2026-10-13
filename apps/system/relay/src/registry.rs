//! Where a relay's peers come from while it runs: the platform's node registry, `GET /v1/nodes`,
//! asked every `EVERY`. `RELAY_PEERS` is the seed a relay starts from, and `registry.json` every
//! node the registry has listed, kept for the next start. A node leaves by being listed `retired`,
//! never by going missing from an answer -- RFC qcyb3. See spec/architecture/registry.md.

use crate::config::{self, Peer};
use crate::mesh::keep;
use crate::relay::Relay;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// The registry's port on each core node's tailnet address. Its `service.toml` states it, and a
/// test in `main.rs` holds the two together.
pub const PORT: u16 = 12013;

/// How often the registry is asked, and so about how long a node written into it waits to be
/// dialed.
pub const EVERY: Duration = Duration::from_secs(30);

/// One registry's time to answer before the next is asked; a node that runs none may drop the
/// request rather than refuse it, and a round asks several within `EVERY`.
const PATIENCE: Duration = Duration::from_secs(3);

/// The last answer taken, in the data directory beside the runs.
pub const FILE: &str = "registry.json";

/// The version of `registry.json`, read first; workspace spec/json.md, "A contract has a version,
/// and it is the first thing read". Version 1 held the last answer of `GET /nodes`, and is still
/// read, once, and carried over; see `cached`.
const VERSION: u32 = 2;

/// A node's spec as the registry lists it, with what a relay reads of it; its other keys are
/// passed over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
	/// Its tailnet IPv4.
	pub address: String,
	/// `compute`, `storage` and `core`, as words; absent from a node written before them, whose
	/// `core` says instead. See spec/architecture/registry.md, "Nodes and apps, as resources of a
	/// kind".
	#[serde(default, skip_serializing_if = "Option::is_none")]
	pub roles: Option<Vec<String>>,
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub core: bool,
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub retired: bool,
}

impl Node {
	/// Whether it is a core node: by `roles` where it has them, `core` ignored, and by `core` where
	/// it has none.
	pub fn is_core(&self) -> bool {
		match &self.roles {
			Some(roles) => roles.iter().any(|role| role == "core"),
			None => self.core,
		}
	}
}

/// One node of `GET /v1/nodes`; its `kind` and `revision` are passed over.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Item {
	pub name: String,
	pub spec: Node,
}

/// What `GET /v1/nodes` answers: every node by name, and the newest change written, 0 when none.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Answer {
	pub items: Vec<Item>,
	pub revision: i64,
}

/// Every node the registry has listed, as last listed, and the revision of the answer that last
/// changed it. A node missing from an answer stays as it was: what `registry.json` keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Taken {
	pub nodes: BTreeMap<String, Node>,
	pub revision: i64,
}

impl Taken {
	/// The nodes held as retired, whose relays are refused.
	pub fn retired(&self) -> BTreeSet<String> {
		self.nodes.iter().filter(|(_, node)| node.retired).map(|(name, _)| name.clone()).collect()
	}
}

/// `answer` taken over `taken`: each node it lists as it lists it, and every other as it was.
fn fold(taken: Option<&Taken>, answer: &Answer) -> Taken {
	let mut nodes = taken.map(|taken| taken.nodes.clone()).unwrap_or_default();
	for item in &answer.items {
		nodes.insert(item.name.clone(), item.spec.clone());
	}
	Taken { nodes, revision: answer.revision }
}

fn peer(name: &str, node: &Node) -> Peer {
	Peer { name: name.to_owned(), address: format!("{}:{}", node.address, config::PORT) }
}

/// The peers `answer` leaves `node` holding after `held`, or none where it is not taken: an answer
/// not listing this node is a registry broken or emptied. Otherwise a node it lists retired is
/// dropped, one it lists otherwise is held at the address listed, on the relay's own port, and one
/// it does not list stays as held -- RFC qcyb3's policy C.
pub fn next_peers(held: &BTreeMap<String, Peer>, answer: &Answer, node: &str) -> Option<Vec<Peer>> {
	if !answer.items.iter().any(|item| item.name == node) {
		return None;
	}
	let mut next = held.clone();
	for item in answer.items.iter().filter(|item| item.name != node) {
		if item.spec.retired {
			next.remove(&item.name);
		} else {
			next.insert(item.name.clone(), peer(&item.name, &item.spec));
		}
	}
	Some(next.into_values().collect())
}

/// The peers a relay starts from: `given`, from `RELAY_PEERS`, with every node the registry has
/// listed over it, at the address listed, and none it holds as retired.
pub fn seeded(given: &[Peer], taken: Option<&Taken>, node: &str) -> Vec<Peer> {
	let mut peers: BTreeMap<String, Peer> =
		given.iter().map(|peer| (peer.name.clone(), peer.clone())).collect();
	for (name, listed) in taken.into_iter().flat_map(|taken| &taken.nodes) {
		if listed.retired {
			peers.remove(name);
		} else if name != node {
			peers.insert(name.clone(), peer(name, listed));
		}
	}
	peers.into_values().collect()
}

/// What changes from `held` to `next`: the peers to dial, and the names to stop dialing. A peer
/// whose address changed is in both, so it is dialed again at the new one.
fn changed(held: &BTreeMap<String, Peer>, next: &[Peer]) -> (Vec<Peer>, Vec<String>) {
	let named: BTreeMap<&str, &Peer> = next.iter().map(|peer| (peer.name.as_str(), peer)).collect();
	let dialed = next.iter().filter(|peer| held.get(&peer.name) != Some(*peer)).cloned().collect();
	let stopped = held
		.values()
		.filter(|peer| named.get(peer.name.as_str()) != Some(peer))
		.map(|peer| peer.name.clone())
		.collect();
	(dialed, stopped)
}

/// The registries to ask, by node and host, in order: every core node listed and not retired, then
/// every other peer held.
fn candidates(taken: Option<&Taken>, held: &BTreeMap<String, Peer>) -> Vec<(String, String)> {
	let core = taken
		.into_iter()
		.flat_map(|taken| &taken.nodes)
		.filter(|(_, listed)| listed.is_core() && !listed.retired)
		.map(|(name, listed)| (name.clone(), listed.address.clone()));
	let mut asked: Vec<(String, String)> = core.collect();
	for peer in held.values() {
		if !asked.iter().any(|(name, _)| *name == peer.name) {
			let host = peer.address.rsplit_once(':').map_or(peer.address.as_str(), |(host, _)| host);
			asked.push((peer.name.clone(), host.to_owned()));
		}
	}
	asked
}

#[derive(Debug, thiserror::Error)]
pub enum AskError {
	#[error("the request could not be formed: {0}")]
	Unformed(#[from] hyper::http::Error),
	#[error("it did not answer: {0}")]
	Unreachable(#[from] hyper_util::client::legacy::Error),
	#[error("its answer broke off: {0}")]
	Broken(#[from] hyper::Error),
	#[error("it did not answer within {0:?}")]
	Slow(Duration),
	#[error("it refused with `{code}`: {message}")]
	Refused { code: String, message: String },
	#[error("it answered what this relay cannot read: {0}")]
	Malformed(#[from] serde_json::Error),
}

struct Registry {
	client: Client<HttpConnector, Empty<Bytes>>,
}

impl Registry {
	fn new() -> Self {
		Self { client: Client::builder(TokioExecutor::new()).build(HttpConnector::new()) }
	}

	async fn ask(&self, host: &str) -> Result<Answer, AskError> {
		let request = Request::get(format!("http://{host}:{PORT}/v1/nodes")).body(Empty::new())?;
		let asked = async {
			let answer = self.client.request(request).await?;
			Ok::<_, AskError>(answer.into_body().collect().await?.to_bytes())
		};
		let body =
			tokio::time::timeout(PATIENCE, asked).await.map_err(|_| AskError::Slow(PATIENCE))??;
		match serde_json::from_slice(&body)? {
			response::Envelope::Success { data } => Ok(data),
			response::Envelope::Error { code, message } => Err(AskError::Refused { code, message }),
		}
	}

	/// The first answer among `candidates`, asked in turn, and the node that gave it; or the last
	/// refusal, none where there was nobody to ask.
	async fn first(
		&self,
		candidates: &[(String, String)],
	) -> Result<(String, Answer), Option<AskError>> {
		let mut last = None;
		for (node, host) in candidates {
			match self.ask(host).await {
				Ok(answer) => return Ok((node.clone(), answer)),
				Err(error) => last = Some(error),
			}
		}
		Err(last)
	}
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
	#[error("{0}")]
	Io(#[from] std::io::Error),
	#[error("it cannot be read: {0}")]
	Malformed(#[from] serde_json::Error),
	#[error("it is version {0}, neither 1 nor {VERSION}")]
	Version(u32),
}

#[derive(Deserialize)]
struct Versioned {
	version: u32,
}

#[derive(Serialize, Deserialize)]
struct Cached {
	version: u32,
	nodes: BTreeMap<String, Node>,
	revision: i64,
}

/// Version 1, the last answer of `GET /nodes`; its `change` is passed over.
#[derive(Deserialize)]
struct CachedOne {
	nodes: BTreeMap<String, Node>,
}

/// What was last taken, from `data`; none before the first. A version-1 file is carried over at
/// revision 0, so the first answer is taken whatever its revision, and is written as version 2.
pub fn cached(data: &Path) -> Result<Option<Taken>, CacheError> {
	let bytes = match std::fs::read(data.join(FILE)) {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error.into()),
	};
	let Versioned { version } = serde_json::from_slice(&bytes)?;
	match version {
		1 => {
			let CachedOne { nodes } = serde_json::from_slice(&bytes)?;
			Ok(Some(Taken { nodes, revision: 0 }))
		}
		VERSION => {
			let Cached { nodes, revision, .. } = serde_json::from_slice(&bytes)?;
			Ok(Some(Taken { nodes, revision }))
		}
		other => Err(CacheError::Version(other)),
	}
}

/// `taken` written to `data` whole or not at all: a relay stopped mid-write starts from the last.
fn cache(data: &Path, taken: &Taken) -> Result<(), CacheError> {
	let record = Cached { version: VERSION, nodes: taken.nodes.clone(), revision: taken.revision };
	let partial = data.join(format!("{FILE}.partial"));
	std::fs::write(&partial, serde_json::to_vec(&record)?)?;
	std::fs::rename(&partial, data.join(FILE))?;
	Ok(())
}

/// Holds a socket to each of `seed`, and asks the registry every `EVERY` for as long as the relay
/// runs, taking each answer with a revision other than the one taken. `taken` is what the seed was
/// read from, if anything.
pub async fn follow(relay: Arc<Relay>, data: PathBuf, seed: Vec<Peer>, mut taken: Option<Taken>) {
	let registry = Registry::new();
	let mut held = BTreeMap::new();
	let mut tasks = BTreeMap::new();
	dial(&relay, seed, &mut held, &mut tasks);
	let mut failing = false;
	let mut refused = None;
	let mut ticks = tokio::time::interval(EVERY);
	ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	loop {
		ticks.tick().await;
		let (from, answer) = match registry.first(&candidates(taken.as_ref(), &held)).await {
			Ok(answered) => answered,
			Err(error) => {
				if !failing {
					let why = error.map(|error| format!(", the last: {error}")).unwrap_or_default();
					eprintln!("relay: no registry answered{why}");
					failing = true;
				}
				continue;
			}
		};
		failing = false;
		if taken.as_ref().is_some_and(|taken| taken.revision == answer.revision) {
			continue;
		}
		let Some(next) = next_peers(&held, &answer, relay.node()) else {
			if refused != Some(answer.revision) {
				eprintln!("relay: not taking revision {} from {from}'s registry", answer.revision);
				refused = Some(answer.revision);
			}
			continue;
		};
		let folded = fold(taken.as_ref(), &answer);
		if let Err(error) = cache(&data, &folded) {
			eprintln!("relay: writing {FILE}: {error}");
		}
		relay.set_retired(folded.retired());
		let (dialed, stopped) = changed(&held, &next);
		let added = dialed.iter().filter(|peer| !held.contains_key(&peer.name));
		let added: Vec<&str> = added.map(|peer| peer.name.as_str()).collect();
		let removed = stopped.iter().filter(|name| !next.iter().any(|peer| peer.name == **name));
		let removed: Vec<&str> = removed.map(String::as_str).collect();
		let moved = dialed.iter().filter(|peer| held.contains_key(&peer.name));
		let moved: Vec<&str> = moved.map(|peer| peer.name.as_str()).collect();
		eprintln!(
			"relay: took revision {} from {from}'s registry, adding [{}], removing [{}], moving [{}]",
			answer.revision,
			added.join(" "),
			removed.join(" "),
			moved.join(" ")
		);
		for name in &stopped {
			held.remove(name);
			if let Some(task) = tasks.remove(name) {
				task.abort();
			}
		}
		dial(&relay, dialed, &mut held, &mut tasks);
		relay.set_peers(held.keys().cloned().collect());
		taken = Some(folded);
	}
}

/// A socket held to each of `peers`, its task kept to be stopped.
fn dial(
	relay: &Arc<Relay>,
	peers: Vec<Peer>,
	held: &mut BTreeMap<String, Peer>,
	tasks: &mut BTreeMap<String, JoinHandle<()>>,
) {
	for peer in peers {
		tasks.insert(peer.name.clone(), tokio::spawn(keep(relay.clone(), peer.clone())));
		held.insert(peer.name.clone(), peer);
	}
}

#[cfg(test)]
mod tests;
