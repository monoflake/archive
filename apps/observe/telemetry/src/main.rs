//! `telemetry` over HTTP: which services run, how the machine under them is, how much work they
//! take on -- public, read-only, and reached through the API gateway under the `telemetry` scope.
//! See spec/architecture/telemetry.md.

use std::path::PathBuf;
use std::sync::Arc;
use telemetry::api::{self, AppState};
use telemetry::ledger::Network as LedgerNetwork;
use telemetry::meter::{self, Meter};
use telemetry::services::Store;

/// musl's allocator is slow under many small allocations, and images are built for speed; see
/// infra's spec/architecture/host.md, "An image is built for speed, and for any node of its
/// architecture".
#[global_allocator]
static ALLOCATOR: mimalloc::MiMalloc = mimalloc::MiMalloc;

/// This service's port, inside its container and everywhere else. `service.toml` states it for
/// host, and the test below holds the two together.
const PORT: u16 = 19570;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
	let data = PathBuf::from(std::env::var("TELEMETRY_DATA").unwrap_or_else(|_| "/data".into()));
	let socket = PathBuf::from(
		std::env::var("TELEMETRY_METER_SOCKET").unwrap_or_else(|_| meter::DEFAULT_SOCKET.into()),
	);
	let listen = std::env::var("LISTEN").unwrap_or_else(|_| format!("0.0.0.0:{PORT}"));

	let state = AppState {
		meter: Arc::new(Meter::new(socket)),
		ledger: Arc::new(LedgerNetwork::new()),
		services: Arc::new(Store::new(&data)),
	};
	let listener = tokio::net::TcpListener::bind(&listen).await?;
	eprintln!("telemetry: listening on {listen}, reading {}", data.display());
	axum::serve(listener, api::routes(state)).with_graceful_shutdown(stopped()).await?;
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
