//! What a relay is told by its environment: which node it is, by id, the relays it holds a socket
//! to until control names them, the secret they share, the token that reads its own host, where and
//! how it reads control, and where its data directory is. See spec/architecture/control.md, "Who
//! reads it, and with what", and RFC o7eir.

use crate::egress;
use hyper::Uri;
use std::collections::BTreeSet;
use std::path::PathBuf;

/// This service's port, inside its container and published on the machine. `service.toml` states
/// it for host, and a test in `main.rs` holds the two together.
pub const PORT: u16 = 12012;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Peer {
	/// Its node's id.
	pub id: String,
	/// `host:port` on the tailnet.
	pub address: String,
}

#[derive(Debug, thiserror::Error, PartialEq, Eq)]
pub enum Invalid {
	#[error("`{0}` is not set")]
	Missing(&'static str),
	#[error("`{0}` in RELAY_PEERS is not `id=host:port`")]
	Peer(String),
	#[error("`{0}` is named twice in RELAY_PEERS")]
	Twice(String),
	#[error(transparent)]
	Egress(#[from] egress::Malformed),
}

#[derive(Debug, Clone)]
pub struct Config {
	/// `NODE_ID`, given by host over whatever the app's own files say; `NODE`, the hostname, is not
	/// read.
	pub node: String,
	/// `RELAY_PEERS`, the seed `registry::seeded` starts from.
	pub peers: Vec<Peer>,
	pub secret: String,
	pub read_token: String,
	pub control: Control,
	pub listen: String,
	/// The directory `[data]` in `service.toml` mounts, or another for a run off the node.
	pub data: PathBuf,
}

/// Where control is read, with what, and how the node is left for it.
#[derive(Debug, Clone)]
pub struct Control {
	/// `CONTROL`, without a trailing slash.
	pub url: String,
	/// `CONTROL_READ_TOKEN`.
	pub token: String,
	/// `EGRESS_PROXIES`, none on a node with IPv4.
	pub proxies: Vec<Uri>,
}

impl Config {
	pub fn from_env() -> Result<Self, Invalid> {
		let required = |name: &'static str| {
			std::env::var(name).ok().filter(|value| !value.is_empty()).ok_or(Invalid::Missing(name))
		};
		let node = required("NODE_ID")?;
		let peers = peers(&std::env::var("RELAY_PEERS").unwrap_or_default(), &node)?;
		let control = Control {
			url: required("CONTROL")?.trim_end_matches('/').to_owned(),
			token: required("CONTROL_READ_TOKEN")?,
			proxies: egress::proxies(&std::env::var("EGRESS_PROXIES").unwrap_or_default())?,
		};
		Ok(Self {
			peers,
			secret: required("RELAY_SECRET")?,
			read_token: required("HOST_READ_TOKEN")?,
			control,
			listen: std::env::var("LISTEN").unwrap_or_else(|_| format!("0.0.0.0:{PORT}")),
			data: std::env::var("RELAY_DATA").unwrap_or_else(|_| "/data".into()).into(),
			node,
		})
	}
}

/// `id=host:port`, separated by spaces. This node's own entry is passed over, so every node can be
/// given the same list.
pub fn peers(value: &str, node: &str) -> Result<Vec<Peer>, Invalid> {
	let mut named = BTreeSet::new();
	let mut peers = Vec::new();
	for entry in value.split_whitespace() {
		let malformed = || Invalid::Peer(entry.to_owned());
		let (id, address) = entry.split_once('=').ok_or_else(malformed)?;
		let (host, port) = address.rsplit_once(':').ok_or_else(malformed)?;
		if id.is_empty() || host.is_empty() || port.parse::<u16>().is_err() {
			return Err(malformed());
		}
		if !named.insert(id) {
			return Err(Invalid::Twice(id.to_owned()));
		}
		if id != node {
			peers.push(Peer { id: id.to_owned(), address: address.to_owned() });
		}
	}
	Ok(peers)
}

#[cfg(test)]
mod tests {
	use super::*;

	const TYO: &str = "d132b0f553e3dcc3";
	const RDU: &str = "d62c82b23a858876";
	const BUF: &str = "e38f403f033d87f7";

	#[test]
	fn peers_are_addresses_by_id_and_this_node_is_passed_over() {
		let given = format!(" {TYO}=tyo.test:12012  {RDU}=rdu.test:12012\t{BUF}=buf.test:12012 ");
		assert_eq!(
			peers(&given, RDU).unwrap(),
			[
				Peer { id: TYO.into(), address: "tyo.test:12012".into() },
				Peer { id: BUF.into(), address: "buf.test:12012".into() },
			]
		);
		assert_eq!(super::peers("", RDU), Ok(vec![]));
	}

	#[test]
	fn a_malformed_or_repeated_peer_is_refused() {
		let bad = [
			TYO.to_owned(),
			format!("{TYO}=tyo.test"),
			"=tyo.test:12012".to_owned(),
			format!("{TYO}=:12012"),
			format!("{TYO}=tyo.test:http"),
		];
		for bad in bad {
			assert_eq!(peers(&bad, RDU), Err(Invalid::Peer(bad.clone())), "{bad}");
		}
		let twice = format!("{TYO}=a.test:1 {TYO}=b.test:1");
		assert_eq!(peers(&twice, RDU), Err(Invalid::Twice(TYO.into())));
	}
}
