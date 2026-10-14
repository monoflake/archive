//! ledger: every task any service was asked to do. See spec/architecture/ledger.md.

use ledger_service::api::{self, Shared};
use ledger_service::import;
use ledger_service::store::Store;
use std::sync::Arc;

/// musl's allocator is slow under many small allocations, and images are built for speed; see
/// infra's spec/architecture/host.md, "An image is built for speed, and for any node of its
/// architecture".
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// This service's port, inside its container and everywhere else. `service.toml` states it for
/// host, and the test below holds the two together.
const PORT: u16 = 12010;

/// `ledger` answers; `ledger import <ledger.db>` copies the SQLite it kept before into the
/// database once, and says what it wrote. Both bring the schema up to date first.
#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let url = std::env::var("DATABASE_URL")
		.map_err(|_| anyhow::anyhow!("DATABASE_URL is not set; `mise run database grant` writes it"))?;
	let store = Store::connect(&url, None)?;
	let applied = store.migrate().await?;
	if !applied.is_empty() {
		eprintln!("ledger: applied migrations {applied:?}");
	}

	let arguments: Vec<String> = std::env::args().skip(1).collect();
	if let [verb, path] = &arguments[..]
		&& verb == "import"
	{
		let imported = import::import(&store, std::path::Path::new(path)).await?;
		println!("{}", serde_json::to_string(&imported)?);
		return Ok(());
	}
	if !arguments.is_empty() {
		anyhow::bail!("ledger takes nothing, or `import <ledger.db>`");
	}

	let listen = std::env::var("LISTEN").unwrap_or_else(|_| format!("0.0.0.0:{PORT}"));
	let shared: Shared = Arc::new(store);
	// The scheduler leases runs only where this node's Patroni says it leads; without the address it
	// does not run, as on a copy that is not one of the cores. It pushes a turn only to an executor
	// on `NODE`, which host sets on every container. See spec/architecture/ledger.md.
	match std::env::var("PATRONI_PRIMARY_URL") {
		Ok(gate) => {
			let own = std::env::var("NODE").ok().filter(|node| !node.is_empty());
			if own.is_none() {
				eprintln!("ledger: NODE is not set; no turn is pushed, every executor polls");
			}
			eprintln!("ledger: scheduling where {gate} answers 200");
			tokio::spawn(ledger_service::scheduler::run(shared.clone(), gate, own));
		}
		Err(_) => eprintln!("ledger: PATRONI_PRIMARY_URL is not set; no run is scheduled from here"),
	}
	let listener = tokio::net::TcpListener::bind(&listen).await?;
	eprintln!("ledger: listening on {listen}");
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

#[cfg(test)]
mod tests {
	#[test]
	fn the_port_is_the_one_the_declaration_states() {
		let declaration = include_str!("../service.toml");
		assert!(declaration.lines().any(|line| line.trim() == format!("port = {}", super::PORT)));
	}
}
