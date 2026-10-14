//! Opening a run, and reading one whole.

use super::record::append;
use super::{Class, NewRun, timestamp};
use crate::store::Store;
use jiff::Timestamp;
use ledger::{Level, Record, State, Task};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Serialize)]
pub struct RunRow {
	pub service: String,
	pub id: String,
	pub workflow: String,
	pub class: String,
	pub status: String,
	pub params: Value,
	pub output: Option<Value>,
	pub error: Option<String>,
	pub run_at: Timestamp,
	pub fence: i64,
	pub created_at: Timestamp,
	pub finished_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Serialize)]
pub struct StepRow {
	pub name: String,
	pub kind: String,
	pub status: String,
	pub attempt: i32,
	pub starts: i32,
	pub config: Value,
	pub result: Option<Value>,
	pub error: Option<String>,
	pub deadline: Option<Timestamp>,
	pub wake_at: Option<Timestamp>,
	pub holder: Option<String>,
	pub fence: Option<i64>,
	pub placement: Option<Value>,
	pub started_at: Timestamp,
	pub finished_at: Option<Timestamp>,
}

#[derive(Debug, Clone, Serialize)]
pub struct LeaseRow {
	pub holder: String,
	pub fence: i64,
	pub expires_at: Timestamp,
}

/// What `GET /v1/runs/{service}/{id}` answers.
#[derive(Debug, Clone, Serialize)]
pub struct RunView {
	pub run: RunRow,
	pub steps: Vec<StepRow>,
	pub lease: Option<LeaseRow>,
}

#[derive(Debug, PartialEq)]
pub enum Created {
	New,
	Same,
	/// The id is a run already, of another workflow, class or params.
	Differs,
	/// Only a `quorum` run is built so far.
	Unsupported,
}

impl Store {
	/// Makes the run and its task, queued. The same request again is the same run; another with the
	/// same id is refused. See spec/architecture/ledger.md, "A run, and the node that replays it".
	pub async fn create_run(&self, service: &str, id: &str, new: &NewRun) -> anyhow::Result<Created> {
		if new.class != Class::Quorum {
			return Ok(Created::Unsupported);
		}
		let mut client = self.pool.get().await?;
		let tx = client.transaction().await?;
		let now = Self::clock(&tx).await?;
		let params = if new.params.is_null() { serde_json::json!({}) } else { new.params.clone() };
		let record = Record {
			service: service.to_owned(),
			id: id.to_owned(),
			kind: new.workflow.clone(),
			state: State::Queued,
			caller: new.caller,
			asked_at: timestamp(now),
			started_at: None,
			finished_at: None,
			summary: serde_json::json!({}),
			detail: None,
		};
		let stored =
			crate::store::StoredTask { task: Task { record, parent: None }, updated_at: timestamp(now) };
		tx.execute(
			"INSERT INTO tasks (service, id, kind, state, caller, updated_at, finished_at, asked_at,
				parent_service, parent_id, record)
			VALUES ($1, $2, $3, 'queued', $4, $5, NULL, $5, NULL, NULL, $6) ON CONFLICT DO NOTHING",
			&[
				&service,
				&id,
				&new.workflow,
				&crate::store::caller_word(new.caller),
				&now,
				&serde_json::to_value(&stored)?,
			],
		)
		.await?;
		let made = tx
			.execute(
				"INSERT INTO runs (service, id, workflow, class, status, params, run_at, created_at, updated_at)
				VALUES ($1, $2, $3, $4, 'ready', $5, $6, $6, $6) ON CONFLICT DO NOTHING",
				&[&service, &id, &new.workflow, &new.class.word(), &params, &now],
			)
			.await?;
		if made == 1 {
			append(&tx, service, id, now, "queued", Level::Info, "queued", Value::Null).await?;
			tx.commit().await?;
			return Ok(Created::New);
		}
		let kept = tx
			.query_one(
				"SELECT workflow, class, params FROM runs WHERE service = $1 AND id = $2",
				&[&service, &id],
			)
			.await?;
		let (workflow, class, kept_params): (String, String, Value) =
			(kept.get(0), kept.get(1), kept.get(2));
		tx.rollback().await?;
		let same = workflow == new.workflow && class == new.class.word() && kept_params == params;
		Ok(if same { Created::Same } else { Created::Differs })
	}

	pub async fn run_view(&self, service: &str, id: &str) -> anyhow::Result<Option<RunView>> {
		let client = self.pool.get().await?;
		let Some(row) = client
			.query_opt(
				"SELECT workflow, class, status, params, output, error, run_at, fence, created_at, finished_at
				FROM runs WHERE service = $1 AND id = $2",
				&[&service, &id],
			)
			.await?
		else {
			return Ok(None);
		};
		let run = RunRow {
			service: service.to_owned(),
			id: id.to_owned(),
			workflow: row.get(0),
			class: row.get(1),
			status: row.get(2),
			params: row.get(3),
			output: row.get(4),
			error: row.get(5),
			run_at: timestamp(row.get(6)),
			fence: row.get(7),
			created_at: timestamp(row.get(8)),
			finished_at: row.get::<_, Option<i64>>(9).map(timestamp),
		};
		let steps = client
			.query(
				"SELECT name, kind, status, attempt, starts, config, result, error, deadline, wake_at,
					holder, fence, placement, started_at, finished_at
				FROM steps WHERE service = $1 AND run = $2 ORDER BY started_at, name",
				&[&service, &id],
			)
			.await?
			.into_iter()
			.map(|step| StepRow {
				name: step.get(0),
				kind: step.get(1),
				status: step.get(2),
				attempt: step.get(3),
				starts: step.get(4),
				config: step.get(5),
				result: step.get(6),
				error: step.get(7),
				deadline: step.get::<_, Option<i64>>(8).map(timestamp),
				wake_at: step.get::<_, Option<i64>>(9).map(timestamp),
				holder: step.get(10),
				fence: step.get(11),
				placement: step.get(12),
				started_at: timestamp(step.get(13)),
				finished_at: step.get::<_, Option<i64>>(14).map(timestamp),
			})
			.collect();
		let lease = client
			.query_opt(
				"SELECT holder, fence, expires_at FROM leases WHERE service = $1 AND run = $2",
				&[&service, &id],
			)
			.await?
			.map(|lease| LeaseRow {
				holder: lease.get(0),
				fence: lease.get(1),
				expires_at: timestamp(lease.get(2)),
			});
		Ok(Some(RunView { run, steps, lease }))
	}
}
