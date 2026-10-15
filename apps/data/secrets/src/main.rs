//! secrets: the platform's secrets, sealed by a root key two of the three cores rebuild. See
//! RFC 231yq.

use secrets_service::api::{self, Secrets, Shared};
use secrets_service::ceremony;
use secrets_service::peers::{self, Control, Cores, PORT};
use secrets_service::seal::Seal;
use secrets_service::store::Store;
use secrets_service::tokens::{NodeTokens, ScopeTokens};
use std::sync::Arc;
use std::time::Duration;

/// musl's allocator is slow under many small allocations, and images are built for speed; see
/// infra's spec/architecture/host.md, "An image is built for speed, and for any node of its
/// architecture".
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// Where this core's share is read, `/data/seal` on the node mounted read-only at `/seal`.
const SHARE: &str = "/seal/share";

/// How often a sealed copy asks its peers again.
const ASKING: Duration = Duration::from_secs(5);

/// `secrets init ...` sets the lock once, see ceremony.rs; with no arguments it serves.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let arguments: Vec<String> = std::env::args().skip(1).collect();
	match arguments.first().map(String::as_str) {
		None => serve().await,
		Some("init") => {
			let asked = ceremony::parse(&arguments[1..]).map_err(anyhow::Error::msg)?;
			for name in ceremony::init(&asked).map_err(anyhow::Error::msg)? {
				println!("{name}");
			}
			Ok(())
		}
		Some(other) => anyhow::bail!("secrets takes nothing, or init; not `{other}`"),
	}
}

/// Brings the schema up to date, then answers, sealed until two cores' shares or the operator's key
/// open it. Without `SECRETS_TOKEN` or `SECRETS_SCOPE_TOKENS` it refuses every write, and without
/// `SECRETS_PEER_TOKEN` it neither gives its share nor asks for another.
async fn serve() -> anyhow::Result<()> {
	let url = std::env::var("DATABASE_URL")
		.map_err(|_| anyhow::anyhow!("DATABASE_URL is not set; `mise run database grant` writes it"))?;
	let cores = Cores::parse(&std::env::var("CORES").unwrap_or_default())
		.map_err(|address| anyhow::anyhow!("CORES holds `{address}`, which is not an IPv4"))?;
	let nodes = NodeTokens::parse(&std::env::var("SECRETS_NODE_TOKENS").unwrap_or_default())
		.map_err(|pair| {
			anyhow::anyhow!("SECRETS_NODE_TOKENS holds `{pair}`, which is not id:digest")
		})?;
	let scopes = ScopeTokens::parse(&std::env::var("SECRETS_SCOPE_TOKENS").unwrap_or_default())
		.map_err(|pair| {
			anyhow::anyhow!(
				"SECRETS_SCOPE_TOKENS holds `{pair}`, which is not scope/app:digest, or is two"
			)
		})?;
	let node_scopes = api::node_scopes(&std::env::var("SECRETS_NODE_SCOPES").unwrap_or_default())
		.map_err(|scope| {
			anyhow::anyhow!("SECRETS_NODE_SCOPES holds `{scope}`, which is not a scope")
		})?;
	let control = Control::new(
		&std::env::var("CONTROL").unwrap_or_default(),
		&std::env::var("CONTROL_READ_TOKEN").unwrap_or_default(),
	);
	let store = Store::connect(&url, None)?;
	let applied = store.migrate().await?;
	if !applied.is_empty() {
		eprintln!("secrets: applied migrations {applied:?}");
	}

	let token = std::env::var("SECRETS_TOKEN").unwrap_or_default();
	if token.is_empty() {
		eprintln!("secrets: SECRETS_TOKEN is not set; only a scope's token writes");
	}
	eprintln!("secrets: {} scope token(s) may write", scopes.len());
	let peer_token = std::env::var("SECRETS_PEER_TOKEN").unwrap_or_default();
	if peer_token.is_empty() {
		eprintln!("secrets: SECRETS_PEER_TOKEN is not set; no share is given or asked for");
	}
	if cores.peers.is_empty() {
		eprintln!("secrets: CORES is not set; only the operator's key opens it");
	}
	eprintln!("secrets: {} node(s) may read", nodes.len());
	if control.base.is_empty() || control.token.is_empty() {
		eprintln!("secrets: CONTROL or CONTROL_READ_TOKEN is not set; no node is answered");
	}
	eprintln!("secrets: nodes are given the scopes {}, and no other", node_scopes.join(" "));
	let share = std::env::var("SECRETS_SHARE").unwrap_or_else(|_| SHARE.into());

	let seal = Arc::new(Seal::default());
	let client = peers::client();
	tokio::spawn(peers::unseal(
		seal.clone(),
		client.clone(),
		cores.peers,
		peer_token.clone(),
		share.into(),
		ASKING,
	));
	let listen = std::env::var("LISTEN").unwrap_or_else(|_| format!("0.0.0.0:{PORT}"));
	let shared: Shared = Arc::new(Secrets {
		store,
		seal,
		token,
		scopes,
		peer_token,
		nodes,
		node_scopes,
		control,
		client,
	});
	let listener = tokio::net::TcpListener::bind(&listen).await?;
	eprintln!("secrets: listening on {listen}, sealed");
	axum::serve(listener, api::routes(shared)).with_graceful_shutdown(stopped()).await?;
	Ok(())
}

/// `docker stop` sends SIGTERM, and a process that is PID 1 in its container ignores it unless it
/// asks; without this every deploy would wait out Docker's grace period and then be killed.
async fn stopped() {
	use tokio::signal::unix::{SignalKind, signal};
	// A handler that cannot be installed waits forever, never stopping the service the moment it
	// starts; database's `stopped` is the same.
	let (Ok(mut term), Ok(mut int)) =
		(signal(SignalKind::terminate()), signal(SignalKind::interrupt()))
	else {
		return std::future::pending().await;
	};
	tokio::select! {
		_ = term.recv() => {}
		_ = int.recv() => {}
	}
}
