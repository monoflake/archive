//! What this service asks the other cores over the tailnet: their shares while it is sealed, and
//! the registry which apps are placed on a node, by its API and never by its crate. See RFC 231yq.

use crate::seal::{self, Seal, Share, Written};
use serde::Deserialize;
use serde_json::Value;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// This service's port, and the registry's, on every core. See spec/architecture/services.md, "A
/// service keeps one port".
pub const PORT: u16 = 12014;
pub const REGISTRY_PORT: u16 = 12013;

/// How long one core is waited for before the next is asked.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The cores, by the base addresses this service reaches each one's copy of itself and of the
/// registry at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cores {
	pub peers: Vec<String>,
	pub registries: Vec<String>,
}

impl Cores {
	/// `CORES` read: the cores' tailnet IPv4s separated by whitespace, as host's `REGISTRY` is. This
	/// core's own may be among them: what it answers is a share already held, and is passed over.
	pub fn parse(value: &str) -> Result<Self, String> {
		let addresses = value
			.split_whitespace()
			.map(|address| address.parse::<Ipv4Addr>().map_err(|_| address.to_owned()))
			.collect::<Result<Vec<_>, _>>()?;
		Ok(Self {
			peers: addresses.iter().map(|address| format!("http://{address}:{PORT}")).collect(),
			registries: addresses
				.iter()
				.map(|address| format!("http://{address}:{REGISTRY_PORT}"))
				.collect(),
		})
	}
}

pub fn client() -> reqwest::Client {
	reqwest::Client::builder().timeout(TIMEOUT).build().expect("a client without TLS always builds")
}

/// The `data` of a successful envelope, or None for anything else.
async fn data(response: reqwest::Response) -> Option<Value> {
	if !response.status().is_success() {
		return None;
	}
	let mut envelope: Value = response.json().await.ok()?;
	(envelope["status"] == "success").then(|| envelope["data"].take())
}

/// The share the copy at `base` holds, if it answers with one.
pub async fn share_of(client: &reqwest::Client, base: &str, token: &str) -> Option<Share> {
	let response =
		client.get(format!("{base}/v1/seal/share")).bearer_auth(token).send().await.ok()?;
	let written = Written::deserialize(data(response).await?).ok()?;
	Share::from_written(&written).ok()
}

/// Every share the peers answer with, asked all at once.
pub async fn shares(client: &reqwest::Client, peers: &[String], token: &str) -> Vec<Share> {
	let mut asked = tokio::task::JoinSet::new();
	for base in peers {
		let (client, base, token) = (client.clone(), base.clone(), token.to_owned());
		asked.spawn(async move { share_of(&client, &base, &token).await });
	}
	asked.join_all().await.into_iter().flatten().collect()
}

/// The apps the registry places on `node`, from the first registry to answer; None when none does.
pub async fn placed_apps(
	client: &reqwest::Client,
	registries: &[String],
	node: &str,
) -> Option<Vec<String>> {
	for base in registries {
		// A node's name is three lowercase letters, checked before it is asked about.
		let Ok(response) = client.get(format!("{base}/v1/apps?node={node}")).send().await else {
			continue;
		};
		let Some(listed) = data(response).await else { continue };
		let Some(items) = listed["items"].as_array() else { continue };
		return Some(
			items.iter().filter_map(|item| item["name"].as_str().map(str::to_owned)).collect(),
		);
	}
	None
}

/// This core's share from its file, or why there is none: a file absent is the ordinary state of a
/// core before the ceremony, said once by the caller.
pub fn read_share(path: &Path) -> Result<Share, String> {
	let text =
		std::fs::read_to_string(path).map_err(|error| format!("{}: {error}", path.display()))?;
	let text = zeroize::Zeroizing::new(text);
	Share::from_json(&text).map_err(|why| format!("{}: {why}", path.display()))
}

/// Asks until the lock is open, every `every`: this core's share is read from `path` while it holds
/// none, and the peers asked for theirs, until two agree on a key. The operator opening it by hand
/// ends this as well. Nothing it logs holds a share.
pub async fn unseal(
	seal: Arc<Seal>,
	client: reqwest::Client,
	peers: Vec<String>,
	token: String,
	path: std::path::PathBuf,
	every: Duration,
) {
	let mut said = String::new();
	let mut say = |now: String| {
		if now != said {
			eprintln!("secrets: {now}");
			said = now;
		}
	};
	while seal.is_sealed() {
		if seal.share().is_none() {
			match read_share(&path) {
				Ok(share) => {
					eprintln!("secrets: holding share {} of this core", share.identifier());
					seal.hold(share);
				}
				Err(why) => say(format!("sealed, and holding no share of its own: {why}")),
			}
		}
		let mut held =
			if token.is_empty() { Vec::new() } else { shares(&client, &peers, &token).await };
		held.extend(seal.share());
		if let Some(key) = seal::rebuild(&held) {
			seal.open(key);
			eprintln!("secrets: unsealed");
			return;
		}
		let mut distinct: Vec<u8> = held.iter().map(Share::identifier).collect();
		distinct.sort_unstable();
		distinct.dedup();
		say(format!(
			"sealed, {} distinct share(s) answering, {} needed",
			distinct.len(),
			seal::THRESHOLD
		));
		tokio::time::sleep(every).await;
	}
}

#[cfg(test)]
mod tests;
