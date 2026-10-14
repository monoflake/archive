//! What every write under a lease shares: the lease checked, the task's events and state.

use super::{Checked, Lease, Refused, stage, timestamp};
use crate::store::{get_in, level_word, nanos};
use deadpool_postgres::{GenericClient, Transaction};
use ledger::{Level, State};
use serde_json::Value;

/// The run's lock and whether `node` holds it at `fence`.
pub(super) async fn hold(
	tx: &Transaction<'_>,
	lease: Lease<'_>,
) -> anyhow::Result<Checked<String>> {
	let Lease { service, run, node, fence } = lease;
	let Some(row) = tx
		.query_opt(
			"SELECT r.status, coalesce(l.holder = $3 AND l.fence = $4, false)
			FROM runs r LEFT JOIN leases l ON l.service = r.service AND l.run = r.id
			WHERE r.service = $1 AND r.id = $2 FOR UPDATE OF r",
			&[&service, &run, &node, &fence],
		)
		.await?
	else {
		return Ok(Err(Refused::NoSuchRun));
	};
	if !row.get::<_, bool>(1) {
		return Ok(Err(Refused::Stale));
	}
	Ok(Ok(row.get(0)))
}

/// One event of the task, numbered one past its last when the clock has not moved on.
#[allow(clippy::too_many_arguments)]
pub(super) async fn append(
	tx: &impl GenericClient,
	service: &str,
	task: &str,
	at: i64,
	stage: &str,
	level: Level,
	message: &str,
	data: Value,
) -> anyhow::Result<()> {
	let data = if data.is_null() { serde_json::json!({}) } else { data };
	tx.execute(
		"INSERT INTO events (service, task, seq, at, stage, level, message, data)
		SELECT $1, $2, GREATEST($3::bigint, coalesce((SELECT max(seq) FROM events
			WHERE service = $1 AND task = $2), 0) + 1), $3, $4, $5, $6, $7",
		&[&service, &task, &at, &stage, &level_word(level), &message, &data],
	)
	.await?;
	Ok(())
}

/// The task's state moved with the run's, in the run's transaction.
pub(super) async fn set_state(
	tx: &impl GenericClient,
	service: &str,
	id: &str,
	state: State,
	at: i64,
	detail: Option<&str>,
) -> anyhow::Result<()> {
	let Some(mut stored) = get_in(tx, service, id).await? else { return Ok(()) };
	let record = &mut stored.task.record;
	record.state = state;
	if state == State::Running && record.started_at.is_none() {
		record.started_at = Some(timestamp(at));
	}
	if matches!(state, State::Done | State::Failed) {
		record.finished_at = Some(timestamp(at));
	}
	if let Some(detail) = detail {
		record.detail = Some(detail.to_owned());
	}
	stored.updated_at = timestamp(at);
	let finished = stored.task.record.finished_at.map(nanos);
	let json = serde_json::to_value(&stored)?;
	tx.execute(
		"UPDATE tasks SET state = $3, updated_at = $4, finished_at = $5, record = $6
		WHERE service = $1 AND id = $2",
		&[&service, &id, &crate::store::state_word(state), &at, &finished, &json],
	)
	.await?;
	Ok(())
}

/// A step's row as the coordinator reads it to decide.
pub(super) struct Step {
	pub(super) kind: String,
	pub(super) status: String,
	pub(super) attempt: i32,
	pub(super) starts: i32,
	pub(super) config: Value,
	pub(super) result: Option<Value>,
	pub(super) error: Option<String>,
	pub(super) deadline: Option<i64>,
	pub(super) wake_at: Option<i64>,
}

pub(super) async fn step_of(
	tx: &Transaction<'_>,
	service: &str,
	run: &str,
	name: &str,
) -> anyhow::Result<Option<Step>> {
	Ok(
		tx.query_opt(
			"SELECT kind, status, attempt, starts, config, result, error, deadline, wake_at
			FROM steps WHERE service = $1 AND run = $2 AND name = $3 FOR UPDATE",
			&[&service, &run, &name],
		)
		.await?
		.map(|row| Step {
			kind: row.get(0),
			status: row.get(1),
			attempt: row.get(2),
			starts: row.get(3),
			config: row.get(4),
			result: row.get(5),
			error: row.get(6),
			deadline: row.get(7),
			wake_at: row.get(8),
		}),
	)
}

/// The run goes to `status` until `run_at`, and its lease is let go.
pub(super) async fn release(
	tx: &Transaction<'_>,
	service: &str,
	run: &str,
	status: &str,
	run_at: i64,
	now: i64,
) -> anyhow::Result<()> {
	tx.execute(
		"UPDATE runs SET status = $3, run_at = $4, updated_at = $5 WHERE service = $1 AND id = $2",
		&[&service, &run, &status, &run_at, &now],
	)
	.await?;
	tx.execute("DELETE FROM leases WHERE service = $1 AND run = $2", &[&service, &run]).await?;
	Ok(())
}

pub(super) async fn fail_step(
	tx: &Transaction<'_>,
	service: &str,
	run: &str,
	name: &str,
	error: &str,
	now: i64,
) -> anyhow::Result<()> {
	tx.execute(
		"UPDATE steps SET status = 'failed', error = $4, finished_at = $5
		WHERE service = $1 AND run = $2 AND name = $3",
		&[&service, &run, &name, &error, &now],
	)
	.await?;
	append(tx, service, run, now, &stage(name), Level::Error, error, Value::Null).await
}
