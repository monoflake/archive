//! A run's end, and a turn that stalled, which may be one.

use super::record::{append, hold, set_state};
use super::{Checked, Lease, Refused, STALLS, TURNS, turn_ended};
use crate::store::Store;
use ledger::{Level, State};
use serde_json::Value;

impl Store {
	/// The run's end, and its task's with it. The same end said twice is the same.
	pub async fn finish_run(
		&self,
		lease: Lease<'_>,
		succeeded: bool,
		output: Option<Value>,
		error: Option<String>,
	) -> anyhow::Result<Checked<()>> {
		let Lease { service, run, .. } = lease;
		let mut client = self.pool.get().await?;
		let tx = client.transaction().await?;
		let word = if succeeded { "done" } else { "failed" };
		let Some(status) = tx
			.query_opt(
				"SELECT status FROM runs WHERE service = $1 AND id = $2 FOR UPDATE",
				&[&service, &run],
			)
			.await?
			.map(|row| row.get::<_, String>(0))
		else {
			return Ok(Err(Refused::NoSuchRun));
		};
		if status == word {
			return Ok(Ok(()));
		}
		if matches!(status.as_str(), "done" | "failed") {
			return Ok(Err(Refused::Diverged(format!("the run is {status} already"))));
		}
		if let Err(refused) = hold(&tx, lease).await? {
			return Ok(Err(refused));
		}
		let now = Self::clock(&tx).await?;
		tx.execute(
			"UPDATE runs SET status = $3, output = $4, error = $5, finished_at = $6, updated_at = $6
			WHERE service = $1 AND id = $2",
			&[&service, &run, &word, &output, &error, &now],
		)
		.await?;
		tx.execute("DELETE FROM leases WHERE service = $1 AND run = $2", &[&service, &run]).await?;
		let (state, level) =
			if succeeded { (State::Done, Level::Info) } else { (State::Failed, Level::Error) };
		set_state(&tx, service, run, state, now, error.as_deref()).await?;
		append(&tx, service, run, now, word, level, error.as_deref().unwrap_or(word), Value::Null)
			.await?;
		tx.commit().await?;
		Ok(Ok(()))
	}

	/// A turn that went 5 minutes without a step asking the ledger, told by its holder: kept among
	/// the run's last turns, the run fails `turn_stalled` once `STALLS` of its last `TURNS` stalled,
	/// and is made ready for the next turn otherwise. See RFC 23wgz, "A turn has a limit too".
	pub async fn stall(&self, lease: Lease<'_>) -> anyhow::Result<Checked<Stalled>> {
		let Lease { service, run, .. } = lease;
		let mut client = self.pool.get().await?;
		let tx = client.transaction().await?;
		if let Err(refused) = hold(&tx, lease).await? {
			return Ok(Err(refused));
		}
		let now = Self::clock(&tx).await?;
		let turns: Vec<String> = tx
			.query_one(
				&format!(
					"UPDATE runs SET {}, updated_at = $3 WHERE service = $1 AND id = $2 RETURNING turns",
					turn_ended("stalled")
				),
				&[&service, &run, &now],
			)
			.await?
			.get(0);
		let stalls = turns.iter().filter(|end| *end == "stalled").count();
		let data = serde_json::json!({ "stalls": stalls, "of": turns.len() });
		tx.execute("DELETE FROM leases WHERE service = $1 AND run = $2", &[&service, &run]).await?;
		let stalled = if stalls >= STALLS {
			let error = format!("turn_stalled: {stalls} of its last {TURNS} turns stalled");
			tx.execute(
				"UPDATE runs SET status = 'failed', error = $3, finished_at = $4, updated_at = $4
				WHERE service = $1 AND id = $2",
				&[&service, &run, &error, &now],
			)
			.await?;
			set_state(&tx, service, run, State::Failed, now, Some(&error)).await?;
			append(&tx, service, run, now, "stalled", Level::Error, "the turn stalled", data).await?;
			append(&tx, service, run, now, "failed", Level::Error, &error, Value::Null).await?;
			Stalled::Failed(error)
		} else {
			tx.execute(
				"UPDATE runs SET status = 'ready', run_at = $3 WHERE service = $1 AND id = $2",
				&[&service, &run, &now],
			)
			.await?;
			append(&tx, service, run, now, "stalled", Level::Warn, "the turn stalled", data).await?;
			Stalled::Ready
		};
		tx.commit().await?;
		Ok(Ok(stalled))
	}
}

/// What a stalled turn left of its run.
#[derive(Debug, PartialEq)]
pub enum Stalled {
	/// Ready for the next turn.
	Ready,
	/// Failed `turn_stalled`, with its error.
	Failed(String),
}
