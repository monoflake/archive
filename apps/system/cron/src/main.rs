//! cron: every scheduled job on a node, in one place. See spec/architecture/cron.md.

use cron::reach::{self, Network};
use cron::scheduler::Cron;
use cron::store::{self, Store};
use cron::table;
use std::path::PathBuf;
use std::sync::Arc;

/// musl's allocator is slow under many small allocations, and images are built for speed; see
/// infra's spec/architecture/host.md, "An image is built for speed, and for any node of its
/// architecture".
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// This service's port, inside its container and everywhere else. `service.toml` states it for
/// host, and the test below holds the two together.
const PORT: u16 = 12011;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let directory =
		std::env::var_os("CRON_DATA").map_or_else(|| PathBuf::from("/data"), PathBuf::from);
	let listen = std::env::var("LISTEN").unwrap_or_else(|_| format!("0.0.0.0:{PORT}"));

	let store = Store::open(&directory.join(store::FILE))?;
	let ledger = ledger::Ledger::start();
	let cron = Cron::new(store, ledger.clone(), Arc::new(Network::new()), reach::origin().to_owned());

	// No table yet is an empty one: host writes it on its next deploy.
	let seen = table::modified(&directory);
	match table::read(&directory) {
		Ok(table) => {
			eprintln!("cron: read {} jobs", table.jobs.len());
			cron.load(table, jiff::Timestamp::now());
		}
		Err(error) => eprintln!("cron: no table yet: {error}"),
	}
	tokio::spawn(cron.clone().drive());
	tokio::spawn(cron.clone().watch(directory.clone(), seen));

	let listener = tokio::net::TcpListener::bind(&listen).await?;
	eprintln!("cron: listening on {listen}, keeping {}", directory.display());
	axum::serve(listener, cron::api::routes(cron)).with_graceful_shutdown(stopped()).await?;
	// What the ledger still holds goes with the process unless it is sent now.
	ledger.stopping("cron", ledger::STOPPING).await;
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
