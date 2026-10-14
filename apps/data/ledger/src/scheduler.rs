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

/// One round: lease what is ready and push each turn granted to an executor on `own`, this
/// ledger's node, without waiting for any to answer. Answers how many were leased.
pub async fn round(
	store: &Store,
	http: &reqwest::Client,
	own: Option<&str>,
) -> anyhow::Result<usize> {
	let granted = store.tick().await?;
	let count = granted.len();
	for grant in granted {
		if let Some(url) = doorbell(&grant, own) {
			push(http.clone(), url.to_owned(), grant);
		}
	}
	Ok(count)
}

/// Where a grant is pushed: the `push_url` its executor gave, and only when that executor is on
/// `own`, the node this ledger runs on. The private side reaches no other node's executor -- a
/// scope placed on several nodes goes to any of them -- so every other executor finds its turn by
/// polling, and a ledger that does not know its node pushes nothing. See RFC 23wgz, "The second
/// slice".
pub fn doorbell<'a>(grant: &'a Granted, own: Option<&str>) -> Option<&'a str> {
	let url = grant.push_url.as_deref()?;
	(own == Some(grant.node.as_str())).then_some(url)
}

/// The turn posted to `url`. A failure is logged and nothing more.
fn push(http: reqwest::Client, url: String, grant: Granted) {
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

/// Rounds for ever, each only while `gate` answers that this node, `own`, leads.
pub async fn run(store: Arc<Store>, gate: String, own: Option<String>) {
	let http = client();
	let mut leading = false;
	loop {
		tokio::time::sleep(TICK).await;
		let now = leads(&http, &gate).await;
		if now != leading {
			eprintln!("ledger: scheduler {}", if now { "started: this node leads" } else { "stopped" });
			leading = now;
		}
		if leading && let Err(error) = round(&store, &http, own.as_deref()).await {
			eprintln!("ledger: a scheduler round failed: {error}");
		}
	}
}
