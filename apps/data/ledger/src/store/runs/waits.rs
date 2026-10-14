//! `step.sleep`, `step.sleepUntil` and `step.waitForEvent`, and the events sent to a run.

use super::record::{append, fail_step, hold, release, step_of};
use super::{Checked, Kind, Lease, Refused, millis, stage, timestamp};
use crate::store::Store;
use jiff::Timestamp;
use ledger::Level;
use serde_json::Value;

#[derive(Debug, PartialEq)]
pub enum Slept {
	Done,
	Sleeping { wake_at: Timestamp },
}

#[derive(Debug, PartialEq)]
pub enum Waited {
	Done(Value),
	Failed(String),
	Waiting { wake_at: Timestamp },
}

impl Store {
	/// `step.sleep` and `step.sleepUntil`: the run sleeps until `wake_at`, an instant the executor
	/// fixed when it first asked, and no node holds it meanwhile.
	pub async fn sleep(
		&self,
		lease: Lease<'_>,
		name: &str,
		wake_at: i64,
	) -> anyhow::Result<Checked<Slept>> {
		let Lease { service, run, .. } = lease;
		let mut client = self.pool.get().await?;
		let tx = client.transaction().await?;
		if let Err(refused) = hold(&tx, lease).await? {
			return Ok(Err(refused));
		}
		let now = Self::clock(&tx).await?;
		let wake = match step_of(&tx, service, run, name).await? {
			None => {
				tx.execute(
					"INSERT INTO steps (service, run, name, kind, status, config, wake_at, started_at)
					VALUES ($1, $2, $3, 'sleep', 'sleeping', '{}', $4, $5)",
					&[&service, &run, &name, &wake_at, &now],
				)
				.await?;
				wake_at
			}
			Some(step) if step.kind != Kind::Sleep.word() => {
				return Ok(Err(Refused::Diverged(format!("{name} was a {}, not a sleep", step.kind))));
			}
			Some(step) if step.status == "done" => return Ok(Ok(Slept::Done)),
			Some(step) => step.wake_at.unwrap_or(wake_at),
		};
		if wake <= now {
			tx.execute(
				"UPDATE steps SET status = 'done', finished_at = $4 WHERE service = $1 AND run = $2 AND name = $3",
				&[&service, &run, &name, &now],
			)
			.await?;
			append(&tx, service, run, now, &stage(name), Level::Info, "woke", Value::Null).await?;
			tx.commit().await?;
			return Ok(Ok(Slept::Done));
		}
		append(
			&tx,
			service,
			run,
			now,
			&stage(name),
			Level::Info,
			"sleeping",
			serde_json::json!({ "wake_at": timestamp(wake) }),
		)
		.await?;
		release(&tx, service, run, "sleeping", wake, now).await?;
		tx.commit().await?;
		Ok(Ok(Slept::Sleeping { wake_at: timestamp(wake) }))
	}

	/// `step.waitForEvent`: an event of `event_type` sent to the run, taken once, oldest first; or
	/// the run waits, holding nothing, until one is sent or `timeout_ms` passes.
	pub async fn wait(
		&self,
		lease: Lease<'_>,
		name: &str,
		event_type: &str,
		timeout_ms: u64,
	) -> anyhow::Result<Checked<Waited>> {
		let Lease { service, run, .. } = lease;
		let mut client = self.pool.get().await?;
		let tx = client.transaction().await?;
		if let Err(refused) = hold(&tx, lease).await? {
			return Ok(Err(refused));
		}
		let now = Self::clock(&tx).await?;
		let (wake, wanted) = match step_of(&tx, service, run, name).await? {
			None => {
				let wake = now.saturating_add(millis(timeout_ms));
				tx.execute(
					"INSERT INTO steps (service, run, name, kind, status, config, wake_at, started_at)
					VALUES ($1, $2, $3, 'wait', 'waiting', $4, $5, $6)",
					&[
						&service,
						&run,
						&name,
						&serde_json::json!({ "event_type": event_type, "timeout_ms": timeout_ms }),
						&wake,
						&now,
					],
				)
				.await?;
				(wake, event_type.to_owned())
			}
			Some(step) if step.kind != Kind::Wait.word() => {
				return Ok(Err(Refused::Diverged(format!("{name} was a {}, not a wait", step.kind))));
			}
			Some(step) if step.status == "done" => {
				return Ok(Ok(Waited::Done(step.result.unwrap_or(Value::Null))));
			}
			Some(step) if step.status == "failed" => {
				return Ok(Ok(Waited::Failed(step.error.unwrap_or_default())));
			}
			Some(step) => (
				step.wake_at.unwrap_or(now),
				step.config["event_type"].as_str().unwrap_or(event_type).to_owned(),
			),
		};
		if let Some(event) = tx
			.query_opt(
				"SELECT key, payload, at FROM run_events
				WHERE service = $1 AND run = $2 AND event_type = $3 AND taken_by IS NULL
				ORDER BY at, key LIMIT 1 FOR UPDATE",
				&[&service, &run, &wanted],
			)
			.await?
		{
			let (key, payload, at): (String, Value, i64) = (event.get(0), event.get(1), event.get(2));
			tx.execute(
				"UPDATE run_events SET taken_by = $4 WHERE service = $1 AND run = $2 AND key = $3",
				&[&service, &run, &key, &name],
			)
			.await?;
			let taken = serde_json::json!({ "type": wanted, "payload": payload, "at": timestamp(at) });
			tx.execute(
				"UPDATE steps SET status = 'done', result = $4, finished_at = $5
				WHERE service = $1 AND run = $2 AND name = $3",
				&[&service, &run, &name, &taken, &now],
			)
			.await?;
			append(
				&tx,
				service,
				run,
				now,
				&stage(name),
				Level::Info,
				"event taken",
				serde_json::json!({ "key": key }),
			)
			.await?;
			tx.commit().await?;
			return Ok(Ok(Waited::Done(taken)));
		}
		if wake <= now {
			let error = format!("timed out waiting for an event of type {wanted}");
			fail_step(&tx, service, run, name, &error, now).await?;
			tx.commit().await?;
			return Ok(Ok(Waited::Failed(error)));
		}
		append(
			&tx,
			service,
			run,
			now,
			&stage(name),
			Level::Info,
			"waiting",
			serde_json::json!({ "event_type": wanted, "until": timestamp(wake) }),
		)
		.await?;
		release(&tx, service, run, "waiting", wake, now).await?;
		tx.commit().await?;
		Ok(Ok(Waited::Waiting { wake_at: timestamp(wake) }))
	}

	/// An event sent to a run, kept once by its `key`. One a waiting run is waiting for makes it
	/// ready now. `None` when there is no such run; else whether the event was new.
	pub async fn send_event(
		&self,
		service: &str,
		run: &str,
		key: &str,
		event_type: &str,
		payload: &Value,
	) -> anyhow::Result<Option<bool>> {
		let mut client = self.pool.get().await?;
		let tx = client.transaction().await?;
		let Some(status) = tx
			.query_opt(
				"SELECT status FROM runs WHERE service = $1 AND id = $2 FOR UPDATE",
				&[&service, &run],
			)
			.await?
			.map(|row| row.get::<_, String>(0))
		else {
			return Ok(None);
		};
		let now = Self::clock(&tx).await?;
		let inserted = tx
			.execute(
				"INSERT INTO run_events (service, run, key, event_type, payload, at)
				VALUES ($1, $2, $3, $4, $5, $6) ON CONFLICT DO NOTHING",
				&[&service, &run, &key, &event_type, payload, &now],
			)
			.await?
			== 1;
		if inserted {
			append(
				&tx,
				service,
				run,
				now,
				"event",
				Level::Info,
				event_type,
				serde_json::json!({ "key": key }),
			)
			.await?;
			if status == "waiting"
				&& tx
					.query_opt(
						"SELECT 1 FROM steps WHERE service = $1 AND run = $2 AND status = 'waiting'
						AND config ->> 'event_type' = $3",
						&[&service, &run, &event_type],
					)
					.await?
					.is_some()
			{
				tx.execute(
					"UPDATE runs SET status = 'ready', run_at = $3, updated_at = $3 WHERE service = $1 AND id = $2",
					&[&service, &run, &now],
				)
				.await?;
			}
		}
		tx.commit().await?;
		Ok(Some(inserted))
	}
}
