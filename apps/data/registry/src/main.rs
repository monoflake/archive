//! registry: the platform's own record of its nodes and its apps. See
//! spec/architecture/registry.md.

use registry_service::api::{self, Registry, Shared};
use registry_service::store::Store;
use std::sync::Arc;

/// musl's allocator is slow under many small allocations, and images are built for speed; see
/// infra's spec/architecture/host.md, "An image is built for speed, and for any node of its
/// architecture".
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// This service's port, inside its container and everywhere else. `service.toml` states it for
/// host, and the test below holds the two together.
const PORT: u16 = 12013;

/// Brings the schema up to date, then answers. Without `REGISTRY_TOKEN` it still answers reads and
/// refuses every write.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let url = std::env::var("DATABASE_URL")
		.map_err(|_| anyhow::anyhow!("DATABASE_URL is not set; `mise run database grant` writes it"))?;
	if std::env::args().len() > 1 {
		anyhow::bail!("registry takes nothing");
	}
	let store = Store::connect(&url, None)?;
	let applied = store.migrate().await?;
	if !applied.is_empty() {
		eprintln!("registry: applied migrations {applied:?}");
	}

	let token = std::env::var("REGISTRY_TOKEN").unwrap_or_default();
	if token.is_empty() {
		eprintln!("registry: REGISTRY_TOKEN is not set; writes are closed");
	}
	let listen = std::env::var("LISTEN").unwrap_or_else(|_| format!("0.0.0.0:{PORT}"));
	// `LEDGER_URL` when it is set, the platform's ledger otherwise, as every service tells it.
	let ledger = Some(ledger::Ledger::start());
	let secrets = registry_service::secrets::Secrets::from_env();
	let shared: Shared = Arc::new(Registry { store, token, ledger, secrets });
	let listener = tokio::net::TcpListener::bind(&listen).await?;
	eprintln!("registry: listening on {listen}");
	axum::serve(listener, api::routes(shared.clone())).with_graceful_shutdown(stopped()).await?;
	// What a write handed the ledger just before the stop goes with it, unless it is sent now.
	registry_service::task::stopping(shared.ledger.as_ref(), registry_service::task::FLUSH).await;
	Ok(())
}

/// `docker stop` sends SIGTERM, and a process that is PID 1 in its container ignores it unless it
/// asks; without this every deploy would wait out Docker's grace period and then be killed.
async fn stopped() {
	let interrupted = async {
		let _ = tokio::signal::ctrl_c().await;
	};
	let terminated = async {
		if let Ok(mut signal) =
			tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
		{
			signal.recv().await;
		}
	};
	tokio::select! {
		() = interrupted => {}
		() = terminated => {}
	}
}

#[cfg(test)]
mod tests {
	#[test]
	fn the_port_is_the_one_the_declaration_states() {
		let declaration = include_str!("../service.toml");
		assert!(declaration.lines().any(|line| line.trim() == format!("port = {}", super::PORT)));
	}
}
