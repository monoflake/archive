//! The scheduler: leases a run to an executor and tells it so. It runs in every copy of the ledger
//! and acts only in the one whose node leads the database, so it follows the primary Patroni chose
//! and adds no election of its own. See RFC 23wgz and spec/architecture/ledger.md, "A run, and the
//! node that replays it".

use crate::store::Store;
use crate::store::runs::Granted;
use std::sync::Arc;
use std::time::Duration;

/// How often a round runs. A round that finds nothing to do reads and writes nothing.
const TICK: Duration = Duration::from_secs(1);

/// A push is best effort and short: an executor that does not take it finds its lease by polling.
const PUSH: Duration = Duration::from_secs(2);

/// Whether this node leads the database: `url` is its own Patroni's `/primary`, which answers 200
/// on the leader and 503 on the rest. Anything else, an unreachable Patroni included, is no.
pub async fn leads(http: &reqwest::Client, url: &str) -> bool {
	match http.get(url).send().await {
		Ok(answer) => answer.status() == reqwest::StatusCode::OK,
		Err(_) => false,
	}
}

pub fn client() -> reqwest::Client {
	reqwest::Client::builder().timeout(PUSH).build().unwrap_or_default()
}

/// One round: lease what is ready and push each turn to the node that was given it, without
/// waiting for any to answer. Answers how many were leased.
pub async fn round(store: &Store, http: &reqwest::Client) -> anyhow::Result<usize> {
	let granted = store.tick().await?;
	let count = granted.len();
	for grant in granted {
		push(http.clone(), grant);
	}
	Ok(count)
}

/// The turn posted to the node's `push_url`, if it gave one. A failure is logged and nothing more.
fn push(http: reqwest::Client, grant: Granted) {
	let Some(url) = grant.push_url else { return };
	tokio::spawn(async move {
		let turn = &grant.turn;
		match http.post(&url).json(turn).send().await {
			Ok(answer) if answer.status().is_success() => {}
			Ok(answer) => eprintln!(
				"ledger: {} did not take {}/{}: {}",
				grant.node,
				turn.service,
				turn.run,
				answer.status()
			),
			Err(error) => {
				eprintln!(
					"ledger: could not push {}/{} to {}: {error}",
					turn.service, turn.run, grant.node
				);
			}
		}
	});
}

/// Rounds for ever, each only while `gate` answers that this node leads.
pub async fn run(store: Arc<Store>, gate: String) {
	let http = client();
	let mut leading = false;
	loop {
		tokio::time::sleep(TICK).await;
		let now = leads(&http, &gate).await;
		if now != leading {
			eprintln!("ledger: scheduler {}", if now { "started: this node leads" } else { "stopped" });
			leading = now;
		}
		if leading && let Err(error) = round(&store, &http).await {
			eprintln!("ledger: a scheduler round failed: {error}");
		}
	}
}
