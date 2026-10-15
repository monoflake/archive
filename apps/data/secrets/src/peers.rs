//! What this service asks: the other cores over the tailnet, for their shares while it is sealed,
//! and control over HTTPS, for which apps are placed on a node. See RFC 231yq and RFC o7eir.

use crate::seal::{self, Seal, Share, Written};
use serde::Deserialize;
use serde_json::Value;
use std::net::Ipv4Addr;
use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

/// This service's port on every core. See spec/architecture/services.md, "A service keeps one
/// port".
pub const PORT: u16 = 12014;

/// How long one core, or control, is waited for.
const TIMEOUT: Duration = Duration::from_secs(5);

/// The cores, by the base addresses this service reaches each one's copy of itself at.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Cores {
	pub peers: Vec<String>,
}

impl Cores {
	/// `CORES` read: the cores' tailnet IPv4s separated by whitespace, as host's `REGISTRY` is. This
	/// core's own may be among them: what it answers is a share already held, and is passed over.
	pub fn parse(value: &str) -> Result<Self, String> {
		let addresses = value
			.split_whitespace()
			.map(|address| address.parse::<Ipv4Addr>().map_err(|_| address.to_owned()))
			.collect::<Result<Vec<_>, _>>()?;
		Ok(Self { peers: addresses.iter().map(|address| format!("http://{address}:{PORT}")).collect() })
	}
}

/// Where control is asked which apps a node runs, `CONTROL`, and the bearer it takes,
/// `CONTROL_READ_TOKEN`. An empty base asks nothing. See spec/architecture/control.md, "Who reads
/// it, and with what".
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Control {
	pub base: String,
	pub token: String,
}

impl Control {
	pub fn new(base: &str, token: &str) -> Self {
		Self { base: base.trim().trim_end_matches('/').to_owned(), token: token.trim().to_owned() }
	}
}

/// One client for the cores' plain HTTP and control's HTTPS, its roots compiled in, since the image
/// is built from scratch and has none; ring, as every other client of the platform's is built on.
pub fn client() -> reqwest::Client {
	let roots = rustls::RootCertStore { roots: webpki_roots::TLS_SERVER_ROOTS.to_vec() };
	let provider = Arc::new(rustls::crypto::ring::default_provider());
	let tls = rustls::ClientConfig::builder_with_provider(provider)
		.with_safe_default_protocol_versions()
		.expect("ring supports the safe default versions")
		.with_root_certificates(roots)
		.with_no_client_auth();
	reqwest::Client::builder()
		.timeout(TIMEOUT)
		.tls_backend_preconfigured(tls)
		.build()
		.expect("a client over a TLS configuration built here always builds")
}

/// The `data` of a successful envelope, or None for anything else.
async fn data(answer: reqwest::Response) -> Option<Value> {
	if !answer.status().is_success() {
		return None;
	}
	response::decode(answer.bytes().await.ok()?).ok()
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

/// The apps control places on `node`, a node's id; None when control is not set or does not answer.
pub async fn placed_apps(
	client: &reqwest::Client,
	control: &Control,
	node: &str,
) -> Option<Vec<String>> {
	if control.base.is_empty() {
		return None;
	}
	// A node's id is 16 hexadecimal digits, checked before it is asked about.
	let asked = client.get(format!("{}/v1/registry/apps?node={node}", control.base));
	let response = asked.bearer_auth(&control.token).send().await.ok()?;
	let listed = data(response).await?;
	Some(placed(listed["items"].as_array()?, node))
}

/// The names among `items` whose placements name `node`, as host's `registry::placed` keeps them:
/// an answer that ignored `?node=` must not open every app's secrets to one node.
fn placed(items: &[Value], node: &str) -> Vec<String> {
	let here = |item: &&Value| {
		let placements = item["spec"]["placements"].as_array();
		placements.is_some_and(|placements| placements.iter().any(|placed| placed == node))
	};
	items.iter().filter(here).filter_map(|item| item["name"].as_str().map(str::to_owned)).collect()
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
