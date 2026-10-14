//! A run's end.

use super::record::{append, hold, set_state};
use super::{Checked, Lease, Refused};
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
}
