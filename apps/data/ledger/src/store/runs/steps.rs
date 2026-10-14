//! `step.do`: an attempt begun, and its end.

use super::record::{append, fail_step, hold, release, step_of};
use super::{
	Checked, Kind, Lease, MAX_RESULT, MAX_STARTS, Refused, StepConfig, millis, stage, timestamp,
};
use crate::store::Store;
use jiff::Timestamp;
use ledger::Level;
use serde::Deserialize;
use serde_json::Value;

#[derive(Debug, PartialEq)]
pub enum Begun {
	Done(Value),
	Failed(String),
	Started { attempt: i32, deadline: Timestamp },
}

#[derive(Debug, PartialEq, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Outcome {
	Success,
	Error,
	/// Not retried, Workflows' `NonRetryableError`.
	Fatal,
}

#[derive(Debug, PartialEq)]
pub enum Finished {
	Continue,
	Retry { run_at: Timestamp },
	Fail { error: String },
	TooLarge,
}

impl Store {
	/// The step asked for: its result when it is done, its error when it failed, else an attempt
	/// begun. An attempt begun by a holder since lost is the same attempt again, and does not count
	/// against the retries; one past its deadline is a failed attempt. Commits `locally`: an attempt
	/// lost with a primary is begun again.
	pub async fn begin_attempt(
		&self,
		lease: Lease<'_>,
		name: &str,
		config: &StepConfig,
	) -> anyhow::Result<Checked<Begun>> {
		let Lease { service, run, .. } = lease;
		let mut client = self.pool.get().await?;
		let tx = client.transaction().await?;
		tx.execute("SELECT set_config('synchronous_commit', 'local', true)", &[]).await?;
		if let Err(refused) = hold(&tx, lease).await? {
			return Ok(Err(refused));
		}
		let now = Self::clock(&tx).await?;
		let Some(step) = step_of(&tx, service, run, name).await? else {
			let deadline = now.saturating_add(millis(config.timeout_ms));
			tx.execute(
				"INSERT INTO steps (service, run, name, kind, status, attempt, starts, config, deadline,
					started_at) VALUES ($1, $2, $3, 'do', 'started', 1, 1, $4, $5, $6)",
				&[&service, &run, &name, &serde_json::to_value(config)?, &deadline, &now],
			)
			.await?;
			append(
				&tx,
				service,
				run,
				now,
				&stage(name),
				Level::Info,
				"started",
				serde_json::json!({ "attempt": 1 }),
			)
			.await?;
			tx.commit().await?;
			return Ok(Ok(Begun::Started { attempt: 1, deadline: timestamp(deadline) }));
		};
		if step.kind != Kind::Do.word() {
			return Ok(Err(Refused::Diverged(format!("{name} was a {}, not a step", step.kind))));
		}
		let recorded: StepConfig = serde_json::from_value(step.config.clone())?;
		let (attempt, starts) = match step.status.as_str() {
			"done" => return Ok(Ok(Begun::Done(step.result.unwrap_or(Value::Null)))),
			"failed" => return Ok(Ok(Begun::Failed(step.error.unwrap_or_default()))),
			"retrying" => (step.attempt + 1, step.starts + 1),
			"started" if step.deadline.is_some_and(|deadline| deadline < now) => {
				// Its holder was lost past the time the step was given: a failed attempt.
				if i64::from(step.attempt) > i64::from(recorded.retries.limit) {
					let error = "timed out";
					fail_step(&tx, service, run, name, error, now).await?;
					tx.commit().await?;
					return Ok(Ok(Begun::Failed(error.to_owned())));
				}
				(step.attempt + 1, step.starts + 1)
			}
			"started" => (step.attempt, step.starts + 1),
			other => return Ok(Err(Refused::Diverged(format!("{name} is {other}")))),
		};
		if starts > MAX_STARTS {
			let error = format!("started {MAX_STARTS} times by holders that were lost");
			fail_step(&tx, service, run, name, &error, now).await?;
			tx.commit().await?;
			return Ok(Ok(Begun::Failed(error)));
		}
		let deadline = now.saturating_add(millis(recorded.timeout_ms));
		tx.execute(
			"UPDATE steps SET status = 'started', attempt = $4, starts = $5, deadline = $6, error = NULL
			WHERE service = $1 AND run = $2 AND name = $3",
			&[&service, &run, &name, &attempt, &starts, &deadline],
		)
		.await?;
		let data = serde_json::json!({ "attempt": attempt, "starts": starts });
		append(&tx, service, run, now, &stage(name), Level::Info, "started", data).await?;
		tx.commit().await?;
		Ok(Ok(Begun::Started { attempt, deadline: timestamp(deadline) }))
	}

	/// An attempt's end. The coordinator decides what follows: the next step, a retry after a delay
	/// that ends the turn, or the step's failure. Confirmed before it is answered, so the next step
	/// begins only once this result is kept.
	pub async fn finish_step(
		&self,
		lease: Lease<'_>,
		name: &str,
		attempt: i32,
		outcome: Outcome,
		result: Option<Value>,
		error: Option<String>,
	) -> anyhow::Result<Checked<Finished>> {
		let Lease { service, run, .. } = lease;
		let mut client = self.pool.get().await?;
		let tx = client.transaction().await?;
		if let Err(refused) = hold(&tx, lease).await? {
			return Ok(Err(refused));
		}
		let now = Self::clock(&tx).await?;
		let Some(step) = step_of(&tx, service, run, name).await? else {
			return Ok(Err(Refused::Diverged(format!("{name} was never begun"))));
		};
		if step.status == "done" && step.attempt == attempt && outcome == Outcome::Success {
			return Ok(Ok(Finished::Continue));
		}
		if step.status != "started" || step.attempt != attempt {
			return Ok(Err(Refused::Diverged(format!(
				"{name} is {} at attempt {}, not started at {attempt}",
				step.status, step.attempt
			))));
		}
		if outcome == Outcome::Success {
			let result = result.unwrap_or(Value::Null);
			if serde_json::to_vec(&result)?.len() > MAX_RESULT {
				fail_step(&tx, service, run, name, "the result is larger than 1 MiB", now).await?;
				tx.commit().await?;
				return Ok(Ok(Finished::TooLarge));
			}
			tx.execute(
				"UPDATE steps SET status = 'done', result = $4, error = NULL, finished_at = $5
				WHERE service = $1 AND run = $2 AND name = $3",
				&[&service, &run, &name, &result, &now],
			)
			.await?;
			append(
				&tx,
				service,
				run,
				now,
				&stage(name),
				Level::Info,
				"done",
				serde_json::json!({ "attempt": attempt }),
			)
			.await?;
			tx.commit().await?;
			return Ok(Ok(Finished::Continue));
		}
		let message = error.unwrap_or_else(|| "the step failed".to_owned());
		let recorded: StepConfig = serde_json::from_value(step.config)?;
		if outcome == Outcome::Error
			&& step.attempt <= i32::try_from(recorded.retries.limit).unwrap_or(i32::MAX)
		{
			let run_at = now.saturating_add(millis(recorded.delay_after(step.attempt)));
			tx.execute(
				"UPDATE steps SET status = 'retrying', error = $4 WHERE service = $1 AND run = $2 AND name = $3",
				&[&service, &run, &name, &message],
			)
			.await?;
			append(
				&tx,
				service,
				run,
				now,
				&stage(name),
				Level::Warn,
				&message,
				serde_json::json!({ "attempt": attempt, "retry_at": timestamp(run_at) }),
			)
			.await?;
			release(&tx, service, run, "sleeping", run_at, now).await?;
			tx.commit().await?;
			return Ok(Ok(Finished::Retry { run_at: timestamp(run_at) }));
		}
		fail_step(&tx, service, run, name, &message, now).await?;
		tx.commit().await?;
		Ok(Ok(Finished::Fail { error: message }))
	}
}
