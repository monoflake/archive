//! Where a relay's peers come from while it runs: the platform's control,
//! `GET {CONTROL}/v1/registry/nodes`, asked every `EVERY`, which knows every node by its id.
//! `RELAY_PEERS` is the seed a relay starts from, and `registry.json` every node control has
//! listed, kept for the next start. A node leaves by being listed `retired`, never by going missing
//! from an answer -- RFC qcyb3. See spec/architecture/control.md, "Who reads it, and with what",
//! and RFC o7eir.

use crate::config::{self, Control, Peer};
use crate::egress::{self, Egress};
use crate::mesh::keep;
use crate::relay::Relay;
use bytes::Bytes;
use http_body_util::{BodyExt, Empty};
use hyper::Request;
use hyper::header::{AUTHORIZATION, USER_AGENT};
use hyper_rustls::HttpsConnector;
use hyper_util::client::legacy::Client;
use hyper_util::rt::TokioExecutor;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

/// How often control is asked, and so about how long a node written into it waits to be dialed;
/// spec/architecture/control.md holds every reader's interval to the free plan's quota.
pub const EVERY: Duration = Duration::from_secs(300);

/// Where control lists every node, under its `registry` scope.
const NODES: &str = "/v1/registry/nodes";

/// Control's time to answer, through a proxy where the node has one.
const PATIENCE: Duration = Duration::from_secs(10);

/// The last answer taken, in the data directory beside the runs.
pub const FILE: &str = "registry.json";

/// The version of `registry.json`, read first; workspace spec/json.md, "A contract has a version,
/// and it is the first thing read". Versions 1 and 2 held nodes by name, and are passed over.
const VERSION: u32 = 3;

/// A node's spec as control lists it, with what a relay reads of it; its other keys are passed
/// over.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Node {
	/// Its tailnet IPv4.
	pub address: String,
	#[serde(default, skip_serializing_if = "std::ops::Not::not")]
	pub retired: bool,
}

/// One node of `GET /v1/registry/nodes`; its `kind`, `name` and `revision` are passed over.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Item {
	pub id: String,
	pub spec: Node,
}

/// What `GET /v1/registry/nodes` answers: every node by id, and the newest change written, 0 when
/// none.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct Answer {
	pub items: Vec<Item>,
	pub revision: i64,
}

/// Every node control has listed, by id, as last listed, and the revision of the answer that last
/// changed it. A node missing from an answer stays as it was: what `registry.json` keeps.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Taken {
	pub nodes: BTreeMap<String, Node>,
	pub revision: i64,
}

impl Taken {
	/// The nodes held as retired, whose relays are refused.
	pub fn retired(&self) -> BTreeSet<String> {
		self.nodes.iter().filter(|(_, node)| node.retired).map(|(id, _)| id.clone()).collect()
	}
}

/// `answer` taken over `taken`: each node it lists as it lists it, and every other as it was.
fn fold(taken: Option<&Taken>, answer: &Answer) -> Taken {
	let mut nodes = taken.map(|taken| taken.nodes.clone()).unwrap_or_default();
	for item in &answer.items {
		nodes.insert(item.id.clone(), item.spec.clone());
	}
	Taken { nodes, revision: answer.revision }
}

fn peer(id: &str, node: &Node) -> Peer {
	Peer { id: id.to_owned(), address: format!("{}:{}", node.address, config::PORT) }
}

/// The peers `answer` leaves `node` holding after `held`, or none where it is not taken: an answer
/// not listing this node is control broken or emptied. Otherwise a node it lists retired is
/// dropped, one it lists otherwise is held at the address listed, on the relay's own port, and one
/// it does not list stays as held -- RFC qcyb3's policy C.
pub fn next_peers(held: &BTreeMap<String, Peer>, answer: &Answer, node: &str) -> Option<Vec<Peer>> {
	if !answer.items.iter().any(|item| item.id == node) {
		return None;
	}
	let mut next = held.clone();
	for item in answer.items.iter().filter(|item| item.id != node) {
		if item.spec.retired {
			next.remove(&item.id);
		} else {
			next.insert(item.id.clone(), peer(&item.id, &item.spec));
		}
	}
	Some(next.into_values().collect())
}

/// The peers a relay starts from: `given`, from `RELAY_PEERS`, with every node control has listed
/// over it, at the address listed, and none it holds as retired.
pub fn seeded(given: &[Peer], taken: Option<&Taken>, node: &str) -> Vec<Peer> {
	let mut peers: BTreeMap<String, Peer> =
		given.iter().map(|peer| (peer.id.clone(), peer.clone())).collect();
	for (id, listed) in taken.into_iter().flat_map(|taken| &taken.nodes) {
		if listed.retired {
			peers.remove(id);
		} else if id != node {
			peers.insert(id.clone(), peer(id, listed));
		}
	}
	peers.into_values().collect()
}

/// What changes from `held` to `next`: the peers to dial, and the ids to stop dialing. A peer whose
/// address changed is in both, so it is dialed again at the new one.
fn changed(held: &BTreeMap<String, Peer>, next: &[Peer]) -> (Vec<Peer>, Vec<String>) {
	let by_id: BTreeMap<&str, &Peer> = next.iter().map(|peer| (peer.id.as_str(), peer)).collect();
	let dialed = next.iter().filter(|peer| held.get(&peer.id) != Some(*peer)).cloned().collect();
	let stopped = held
		.values()
		.filter(|peer| by_id.get(peer.id.as_str()) != Some(peer))
		.map(|peer| peer.id.clone())
		.collect();
	(dialed, stopped)
}

#[derive(Debug, thiserror::Error)]
pub enum AskError {
	#[error("the request could not be formed: {0}")]
	Unformed(#[from] hyper::http::Error),
	#[error("it did not answer: {}", egress::chain(.0))]
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

/// Control, asked over TLS with roots compiled in rather than read from the system, as host asks
/// it: an image built from scratch has none.
struct Reader {
	client: Client<HttpsConnector<Egress>, Empty<Bytes>>,
	url: String,
	token: String,
}

impl Reader {
	fn new(control: Control) -> Self {
		let https = hyper_rustls::HttpsConnectorBuilder::new()
			.with_webpki_roots()
			.https_only()
			.enable_http1()
			.wrap_connector(Egress::new(control.proxies));
		let client = Client::builder(TokioExecutor::new()).build(https);
		Self { client, url: control.url, token: control.token }
	}

	async fn ask(&self) -> Result<Answer, AskError> {
		let request = Request::get(nodes(&self.url))
			.header(USER_AGENT, "canmi-relay")
			.header(AUTHORIZATION, format!("Bearer {}", self.token))
			.body(Empty::new())?;
		let asked = async {
			let answer = self.client.request(request).await?;
			Ok::<_, AskError>(answer.into_body().collect().await?.to_bytes())
		};
		let body =
			tokio::time::timeout(PATIENCE, asked).await.map_err(|_| AskError::Slow(PATIENCE))??;
		let envelope: response::Envelope<Answer> = serde_json::from_slice(&body)?;
		envelope
			.into_result()
			.map_err(|response::Failure { code, message }| AskError::Refused { code, message })
	}
}

/// The nodes' address on control at `url`, `CONTROL` without a trailing slash.
fn nodes(url: &str) -> String {
	format!("{url}{NODES}")
}

#[derive(Debug, thiserror::Error)]
pub enum CacheError {
	#[error("{0}")]
	Io(#[from] std::io::Error),
	#[error("it cannot be read: {0}")]
	Malformed(#[from] serde_json::Error),
	#[error("it is version {0}, not {VERSION}, and is passed over")]
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

/// What was last taken, from `data`; none before the first. A file of another version is refused,
/// and the first answer taken writes over it.
pub fn cached(data: &Path) -> Result<Option<Taken>, CacheError> {
	let bytes = match std::fs::read(data.join(FILE)) {
		Ok(bytes) => bytes,
		Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
		Err(error) => return Err(error.into()),
	};
	let Versioned { version } = serde_json::from_slice(&bytes)?;
	if version != VERSION {
		return Err(CacheError::Version(version));
	}
	let Cached { nodes, revision, .. } = serde_json::from_slice(&bytes)?;
	Ok(Some(Taken { nodes, revision }))
}

/// `taken` written to `data` whole or not at all: a relay stopped mid-write starts from the last.
fn cache(data: &Path, taken: &Taken) -> Result<(), CacheError> {
	let record = Cached { version: VERSION, nodes: taken.nodes.clone(), revision: taken.revision };
	let partial = data.join(format!("{FILE}.partial"));
	std::fs::write(&partial, serde_json::to_vec(&record)?)?;
	std::fs::rename(&partial, data.join(FILE))?;
	Ok(())
}

/// Holds a socket to each of `seed`, and asks `control` every `EVERY` for as long as the relay
/// runs, taking each answer with a revision other than the one taken. `taken` is what the seed was
/// read from, if anything.
pub async fn follow(
	relay: Arc<Relay>,
	control: Control,
	data: PathBuf,
	seed: Vec<Peer>,
	mut taken: Option<Taken>,
) {
	let reader = Reader::new(control);
	let mut held = BTreeMap::new();
	let mut tasks = BTreeMap::new();
	dial(&relay, seed, &mut held, &mut tasks);
	let mut failing = false;
	let mut refused = None;
	let mut ticks = tokio::time::interval(EVERY);
	ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	loop {
		ticks.tick().await;
		let answer = match reader.ask().await {
			Ok(answer) => answer,
			Err(error) => {
				if !failing {
					eprintln!("relay: control did not answer: {error}");
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
				eprintln!(
					"relay: not taking revision {} from control, which lists no {}",
					answer.revision,
					relay.node()
				);
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
		let added = dialed.iter().filter(|peer| !held.contains_key(&peer.id));
		let added: Vec<&str> = added.map(|peer| peer.id.as_str()).collect();
		let removed = stopped.iter().filter(|id| !next.iter().any(|peer| peer.id == **id));
		let removed: Vec<&str> = removed.map(String::as_str).collect();
		let moved = dialed.iter().filter(|peer| held.contains_key(&peer.id));
		let moved: Vec<&str> = moved.map(|peer| peer.id.as_str()).collect();
		eprintln!(
			"relay: took revision {} from control, adding [{}], removing [{}], moving [{}]",
			answer.revision,
			added.join(" "),
			removed.join(" "),
			moved.join(" ")
		);
		for id in &stopped {
			held.remove(id);
			if let Some(task) = tasks.remove(id) {
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
		tasks.insert(peer.id.clone(), tokio::spawn(keep(relay.clone(), peer.clone())));
		held.insert(peer.id.clone(), peer);
	}
}

#[cfg(test)]
mod tests;
