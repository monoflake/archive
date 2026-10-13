//! Where a relay's peers come from while it runs: the platform's node registry, asked every
//! `EVERY`. `RELAY_PEERS` is the seed a relay starts from, and `registry.json` the last answer
//! taken, kept for the next start. See spec/architecture/registry.md and RFC ja5hm.

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
use std::collections::BTreeMap;
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
/// and it is the first thing read".
const VERSION: u32 = 1;

/// A node as the registry lists it, with what a relay reads of it; its other keys are passed over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
	/// Its tailnet IPv4.
	pub address: String,
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub core: bool,
}

/// What `GET /nodes` answers: every node by name, and the newest change written, 0 when none.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Answer {
	pub nodes: BTreeMap<String, Node>,
	pub change: i64,
}

/// The peers an answer gives `node`, or none where it is not taken: an answer naming no nodes, or
/// not naming this one, is a registry broken or emptied, and the peers held stay. Otherwise every
/// other node it names is a peer, on the relay's own port. The policy is the author's to revise.
pub fn next_peers(answer: &Answer, node: &str) -> Option<Vec<Peer>> {
	if answer.nodes.is_empty() || !answer.nodes.contains_key(node) {
		return None;
	}
	let others = answer.nodes.iter().filter(|(name, _)| *name != node);
	Some(
		others
			.map(|(name, listed)| Peer {
				name: name.clone(),
				address: format!("{}:{}", listed.address, config::PORT),
			})
			.collect(),
	)
}

/// The peers a relay starts from: `given`, from `RELAY_PEERS`, with the last answer taken over it
/// for a node both name.
pub fn seeded(given: &[Peer], cached: Option<&Answer>, node: &str) -> Vec<Peer> {
	let mut peers: BTreeMap<String, Peer> =
		given.iter().map(|peer| (peer.name.clone(), peer.clone())).collect();
	for peer in cached.and_then(|answer| next_peers(answer, node)).unwrap_or_default() {
		peers.insert(peer.name.clone(), peer);
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

/// The registries to ask, by node and host, in order: every core node of the last answer taken,
/// then every other peer held.
fn candidates(taken: Option<&Answer>, held: &BTreeMap<String, Peer>) -> Vec<(String, String)> {
	let core = taken
		.into_iter()
		.flat_map(|answer| &answer.nodes)
		.filter(|(_, listed)| listed.core)
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
		let request = Request::get(format!("http://{host}:{PORT}/nodes")).body(Empty::new())?;
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
	#[error("it is version {0}, not {VERSION}")]
	Version(u32),
}

#[derive(Deserialize)]
struct Versioned {
	version: u32,
}

#[derive(Serialize, Deserialize)]
struct Cached {
	version: u32,
	#[serde(flatten)]
	answer: Answer,
}

/// The last answer taken, from `data`; none before the first.
pub fn cached(data: &Path) -> Result<Option<Answer>, CacheError> {
	let bytes = match std::fs::read(data.join(FILE)) {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error.into()),
	};
	let Versioned { version } = serde_json::from_slice(&bytes)?;
	if version != VERSION {
		return Err(CacheError::Version(version));
	}
	let Cached { answer, .. } = serde_json::from_slice(&bytes)?;
	Ok(Some(answer))
}

/// `answer` written to `data` whole or not at all: a relay stopped mid-write starts from the last.
fn cache(data: &Path, answer: &Answer) -> Result<(), CacheError> {
	let record = Cached { version: VERSION, answer: answer.clone() };
	let partial = data.join(format!("{FILE}.partial"));
	std::fs::write(&partial, serde_json::to_vec(&record)?)?;
	std::fs::rename(&partial, data.join(FILE))?;
	Ok(())
}

/// Holds a socket to each of `seed`, and asks the registry every `EVERY` for as long as the relay
/// runs, taking each answer with a change other than the one taken. `taken` is the answer the seed
/// was read from, if any.
pub async fn follow(relay: Arc<Relay>, data: PathBuf, seed: Vec<Peer>, mut taken: Option<Answer>) {
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
		if taken.as_ref().is_some_and(|taken| taken.change == answer.change) {
			continue;
		}
		let Some(next) = next_peers(&answer, relay.node()) else {
			if refused != Some(answer.change) {
				eprintln!("relay: not taking change {} from {from}'s registry", answer.change);
				refused = Some(answer.change);
			}
			continue;
		};
		if let Err(error) = cache(&data, &answer) {
			eprintln!("relay: writing {FILE}: {error}");
		}
		let (dialed, stopped) = changed(&held, &next);
		let added = dialed.iter().filter(|peer| !held.contains_key(&peer.name));
		let added: Vec<&str> = added.map(|peer| peer.name.as_str()).collect();
		let removed = stopped.iter().filter(|name| !next.iter().any(|peer| peer.name == **name));
		let removed: Vec<&str> = removed.map(String::as_str).collect();
		let moved = dialed.iter().filter(|peer| held.contains_key(&peer.name));
		let moved: Vec<&str> = moved.map(|peer| peer.name.as_str()).collect();
		eprintln!(
			"relay: took change {} from {from}'s registry, adding [{}], removing [{}], moving [{}]",
			answer.change,
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
		taken = Some(answer);
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
mod tests {
	use super::*;

	fn node(address: &str, core: bool) -> Node {
		Node { address: address.into(), core }
	}

	fn peer(name: &str, address: &str) -> Peer {
		Peer { name: name.into(), address: address.into() }
	}

	fn held(peers: &[Peer]) -> BTreeMap<String, Peer> {
		peers.iter().map(|peer| (peer.name.clone(), peer.clone())).collect()
	}

	fn answer(nodes: &[(&str, Node)], change: i64) -> Answer {
		let nodes = nodes.iter().map(|(name, node)| ((*name).to_owned(), node.clone())).collect();
		Answer { nodes, change }
	}

	/// An answer as the registry gives it, with keys a relay does not read and `core` left out.
	#[test]
	fn reads_the_answer_as_the_registry_gives_it() {
		let body = r#"{ "status": "success", "data": { "change": 7, "nodes": {
			"tyo": { "address": "100.64.0.1", "core": true, "tier": "core", "domain": "tyo.test" },
			"buf": { "address": "100.64.0.2", "system": "linux" } } } }"#;
		let response::Envelope::Success { data } = serde_json::from_str(body).unwrap() else {
			panic!("a success");
		};
		let read: Answer = data;
		let listed = [("tyo", node("100.64.0.1", true)), ("buf", node("100.64.0.2", false))];
		assert_eq!(read, answer(&listed, 7));
	}

	#[test]
	fn every_other_node_named_is_a_peer_on_the_relays_port() {
		let rdu = ("rdu", node("100.64.0.3", true));
		let listed =
			[("tyo", node("100.64.0.1", true)), rdu.clone(), ("buf", node("100.64.0.2", false))];
		assert_eq!(
			next_peers(&answer(&listed, 7), "rdu"),
			Some(vec![peer("buf", "100.64.0.2:12012"), peer("tyo", "100.64.0.1:12012")])
		);
		// The only node named is this one: taken, with no peers.
		assert_eq!(next_peers(&answer(&[rdu], 8), "rdu"), Some(vec![]));
	}

	#[test]
	fn an_answer_naming_no_nodes_is_not_taken() {
		assert_eq!(next_peers(&answer(&[], 0), "rdu"), None);
		assert_eq!(next_peers(&answer(&[], 9), "rdu"), None);
	}

	#[test]
	fn an_answer_not_naming_this_node_is_not_taken() {
		let listed = answer(&[("tyo", node("100.64.0.1", true))], 7);
		assert_eq!(next_peers(&listed, "rdu"), None);
	}

	#[test]
	fn the_last_answer_taken_wins_over_relay_peers() {
		let given = [peer("tyo", "tyo.test:12012"), peer("gvx", "gvx.test:12012")];
		let cached = answer(&[("rdu", node("100.64.0.3", true)), ("tyo", node("100.64.0.1", true))], 7);
		assert_eq!(
			seeded(&given, Some(&cached), "rdu"),
			[peer("gvx", "gvx.test:12012"), peer("tyo", "100.64.0.1:12012")]
		);
		let alone = [peer("gvx", "gvx.test:12012"), peer("tyo", "tyo.test:12012")];
		assert_eq!(seeded(&given, None, "rdu"), alone);
		// A cache that would not be taken now adds nothing.
		assert_eq!(seeded(&given, Some(&answer(&[], 3)), "rdu").len(), 2);
	}

	#[test]
	fn a_peer_moved_is_stopped_and_dialed_again() {
		let held = held(&[peer("tyo", "a:12012"), peer("gvx", "b:12012"), peer("buf", "c:12012")]);
		let next = [peer("tyo", "a:12012"), peer("buf", "d:12012"), peer("sin", "e:12012")];
		let (dialed, stopped) = changed(&held, &next);
		assert_eq!(dialed, [peer("buf", "d:12012"), peer("sin", "e:12012")]);
		assert_eq!(stopped, ["buf", "gvx"]);
	}

	#[test]
	fn the_core_is_asked_first_then_every_other_peer() {
		let held = held(&[peer("tyo", "100.64.0.1:12012"), peer("buf", "100.64.0.2:12012")]);
		let listed = [
			("tyo", node("100.64.0.1", true)),
			("sin", node("100.64.0.4", true)),
			("buf", node("100.64.0.2", false)),
		];
		let taken = answer(&listed, 7);
		let asked = |taken| -> Vec<(String, String)> { candidates(taken, &held) };
		let pairs = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
			pairs.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
		};
		assert_eq!(
			asked(Some(&taken)),
			pairs(&[("sin", "100.64.0.4"), ("tyo", "100.64.0.1"), ("buf", "100.64.0.2")])
		);
		// Before any answer is taken, every peer, from `RELAY_PEERS`.
		assert_eq!(asked(None), pairs(&[("buf", "100.64.0.2"), ("tyo", "100.64.0.1")]));
	}

	#[test]
	fn the_cache_is_read_back_and_a_missing_one_is_none() {
		let data = tempfile::tempdir().unwrap();
		assert!(cached(data.path()).unwrap().is_none());
		let taken = answer(&[("rdu", node("100.64.0.3", true)), ("buf", node("100.64.0.2", false))], 7);
		cache(data.path(), &taken).unwrap();
		assert_eq!(cached(data.path()).unwrap(), Some(taken));
		let written: serde_json::Value =
			serde_json::from_slice(&std::fs::read(data.path().join(FILE)).unwrap()).unwrap();
		assert_eq!(written["version"], VERSION);
		assert!(written["nodes"]["buf"].get("core").is_none());
		let newer = r#"{ "version": 2, "nodes": {}, "change": 1 }"#;
		std::fs::write(data.path().join(FILE), newer).unwrap();
		assert!(matches!(cached(data.path()), Err(CacheError::Version(2))));
	}
}
