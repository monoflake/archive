//! A run: a task that has a workflow, its steps, the events sent to it and the lease it is held
//! under. Every statement here is the coordinator's half of RFC 23wgz; the executors' half is
//! `runs_api`. See spec/architecture/ledger.md, "A run, and the node that replays it".
//!
//! A write that settles something -- a grant, a step's result, a run's end -- commits confirmed, as
//! the cluster's default is. One a lost primary may lose without harm -- a lease's renewal, an
//! executor's heartbeat, an attempt's start -- commits `locally`. See RFC bhybp.

use super::{Store, get_in, level_word, nanos};
use deadpool_postgres::{GenericClient, Transaction};
use jiff::Timestamp;
use ledger::{Caller, Level, Record, State, Task};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// A step's result, at most this many bytes of JSON, Workflows' own limit. A larger one fails the
/// step. See RFC 23wgz.
pub const MAX_RESULT: usize = 1 << 20;

/// A lease lasts this long on the primary's clock and is renewed every `RENEW`, so a holder lost
/// mid-turn is found within half a minute. See RFC 23wgz.
pub const LEASE_NANOS: i64 = 30_000_000_000;
pub const RENEW_SECONDS: u64 = 10;

/// An executor not heard from within this is not given a turn: longer than a poll's 30 seconds.
const ALIVE_NANOS: i64 = 60_000_000_000;

/// A step started this many times by holders that were lost fails, so one that kills its isolate
/// cannot loop for ever. A crash is not an attempt. See RFC 23wgz.
pub const MAX_STARTS: i32 = 20;

/// A turn carries the results of its done steps up to this many bytes; past it `partial` is set
/// and the executor reads the rest one by one.
const TURN_BYTES: i64 = 8 << 20;

/// The primary's own clock, in nanoseconds, read once per statement or transaction: one clock
/// decides every expiry, never a client's. See RFC 23wgz.
const CLOCK: &str = "SELECT (extract(epoch FROM clock_timestamp()) * 1000000000)::bigint";

/// The same as an expression, for a statement that stamps and writes in one round trip.
const NOW: &str = "(extract(epoch FROM clock_timestamp()) * 1000000000)::bigint";

/// The lease a write claims: the run, the node that holds it and the number it was granted under.
#[derive(Debug, Clone, Copy)]
pub struct Lease<'a> {
	pub service: &'a str,
	pub run: &'a str,
	pub node: &'a str,
	pub fence: i64,
}

/// What a run is told about a step when it is first asked for: Workflows' own defaults. See
/// https://developers.cloudflare.com/workflows/build/sleeping-and-retrying/ -- `retries.limit` 5
/// retries after the first attempt, `delay` 10 seconds, `backoff` exponential, `timeout` 10
/// minutes.
const DEFAULT_LIMIT: u32 = 5;
const DEFAULT_DELAY_MS: u64 = 10_000;
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// Workflows limits a step to 10,000 retries.
const MAX_LIMIT: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backoff {
	Constant,
	Linear,
	Exponential,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retries {
	pub limit: u32,
	pub delay_ms: u64,
	pub backoff: Backoff,
}

impl Default for Retries {
	fn default() -> Self {
		Self { limit: DEFAULT_LIMIT, delay_ms: DEFAULT_DELAY_MS, backoff: Backoff::Exponential }
	}
}

/// A step's `retries` and `timeout`, as the code declared them, a missing part taken from the
/// defaults. Kept whole in the step's row, so the schedule of a retry never changes mid-way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepConfig {
	#[serde(default)]
	pub retries: Retries,
	#[serde(default = "default_timeout")]
	pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
	DEFAULT_TIMEOUT_MS
}

impl Default for StepConfig {
	fn default() -> Self {
		Self { retries: Retries::default(), timeout_ms: DEFAULT_TIMEOUT_MS }
	}
}

impl StepConfig {
	pub fn valid(&self) -> bool {
		self.retries.limit <= MAX_LIMIT && self.timeout_ms >= 1
	}

	/// How long to wait before attempt `failed + 1`, in milliseconds.
	pub fn delay_after(&self, failed: i32) -> u64 {
		let n = u64::try_from(failed.max(1)).unwrap_or(1);
		let base = self.retries.delay_ms;
		match self.retries.backoff {
			Backoff::Constant => base,
			Backoff::Linear => base.saturating_mul(n),
			Backoff::Exponential => base.saturating_mul(1u64 << (n - 1).min(32)),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
	Quorum,
	Local,
}

impl Class {
	fn word(self) -> &'static str {
		match self {
			Self::Quorum => "quorum",
			Self::Local => "local",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
	Do,
	Sleep,
	Wait,
}

impl Kind {
	fn word(self) -> &'static str {
		match self {
			Self::Do => "do",
			Self::Sleep => "sleep",
			Self::Wait => "wait",
		}
	}
}

/// What `PUT /v1/runs/{service}/{id}` asks.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NewRun {
	pub workflow: String,
	#[serde(default = "quorum")]
	pub class: Class,
	#[serde(default)]
	pub params: Value,
	#[serde(default = "ours")]
	pub caller: Caller,
}

fn quorum() -> Class {
	Class::Quorum
}

fn ours() -> Caller {
	Caller::Ours
}

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

/// A turn handed to a node: the run to replay, the fence every write must carry, and what its done
/// steps returned, so the replay answers them without asking.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
	pub service: String,
	pub run: String,
	pub workflow: String,
	pub params: Value,
	pub fence: i64,
	pub expires_at: Timestamp,
	pub done: BTreeMap<String, Value>,
	pub partial: bool,
}

/// A turn just granted, and where to push it, when the node said where.
#[derive(Debug, Clone)]
pub struct Granted {
	pub node: String,
	pub push_url: Option<String>,
	pub turn: Turn,
}

pub(crate) fn timestamp(at: i64) -> Timestamp {
	Timestamp::from_nanosecond(i128::from(at)).unwrap_or(Timestamp::UNIX_EPOCH)
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

/// How a write that needs the lease ended when it did not.
#[derive(Debug, PartialEq)]
pub enum Refused {
	NoSuchRun,
	/// The fence or the holder is not the run's current one.
	Stale,
	/// The step is not the kind or in the state the record says.
	Diverged(String),
}

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

/// What `run` and `view` read, a run's whole standing, so a test and the console see one thing.
type Checked<T> = Result<T, Refused>;

impl Store {
	async fn clock(client: &impl GenericClient) -> anyhow::Result<i64> {
		Ok(client.query_one(CLOCK, &[]).await?.get(0))
	}

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
			super::StoredTask { task: Task { record, parent: None }, updated_at: timestamp(now) };
		tx.execute(
			"INSERT INTO tasks (service, id, kind, state, caller, updated_at, finished_at, asked_at,
				parent_service, parent_id, record)
			VALUES ($1, $2, $3, 'queued', $4, $5, NULL, $5, NULL, NULL, $6) ON CONFLICT DO NOTHING",
			&[
				&service,
				&id,
				&new.workflow,
				&super::caller_word(new.caller),
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

	/// An executor's heartbeat, which also names where to push a turn to, and the turns it holds now.
	/// The heartbeat commits `locally`: a lost one only makes a node look absent for a moment.
	pub async fn poll(
		&self,
		node: &str,
		capacity: i32,
		push_url: Option<&str>,
	) -> anyhow::Result<Vec<Turn>> {
		let client = self.pool.get().await?;
		client
			.execute(
				&format!(
					"WITH {} INSERT INTO executors (node, seen_at, capacity, push_url) \
					 SELECT $1, {NOW}, $2, $3 FROM locally \
					 ON CONFLICT (node) DO UPDATE SET seen_at = excluded.seen_at, \
					 capacity = excluded.capacity, push_url = excluded.push_url",
					super::LOCALLY
				),
				&[&node, &capacity, &push_url],
			)
			.await?;
		let now = Self::clock(&client).await?;
		let held = client
			.query(
				"SELECT service, run FROM leases WHERE holder = $1 AND expires_at > $2
				ORDER BY granted_at, service, run",
				&[&node, &now],
			)
			.await?;
		let mut turns = Vec::new();
		for lease in held {
			let (service, run): (String, String) = (lease.get(0), lease.get(1));
			if let Some(turn) = turn_for(&client, &service, &run).await? {
				turns.push(turn);
			}
		}
		Ok(turns)
	}

	/// The lease extended, committed `locally`; it also shows the node is alive.
	pub async fn renew(&self, lease: Lease<'_>) -> anyhow::Result<Option<Timestamp>> {
		let Lease { service, run, node, fence } = lease;
		let client = self.pool.get().await?;
		let row = client
			.query_opt(
				&format!(
					"WITH {}, renewed AS (UPDATE leases SET expires_at = {NOW} + $5 FROM locally \
					 WHERE service = $1 AND run = $2 AND holder = $3 AND fence = $4 \
					 RETURNING expires_at), \
					 seen AS (UPDATE executors SET seen_at = {NOW} WHERE node = $3 \
					 AND EXISTS (SELECT 1 FROM renewed) RETURNING 1) \
					 SELECT expires_at FROM renewed",
					super::LOCALLY
				),
				&[&service, &run, &node, &fence, &LEASE_NANOS],
			)
			.await?;
		Ok(row.map(|row| timestamp(row.get(0))))
	}
}

/// The run's lock and whether `node` holds it at `fence`.
async fn hold(tx: &Transaction<'_>, lease: Lease<'_>) -> anyhow::Result<Checked<String>> {
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
async fn append(
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
async fn set_state(
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
		&[&service, &id, &super::state_word(state), &at, &finished, &json],
	)
	.await?;
	Ok(())
}

/// The turn the run's lease holds, with what its done steps returned.
async fn turn_for(
	client: &impl GenericClient,
	service: &str,
	run: &str,
) -> anyhow::Result<Option<Turn>> {
	let Some(row) = client
		.query_opt(
			"SELECT r.workflow, r.params, l.fence, l.expires_at
			FROM runs r JOIN leases l ON l.service = r.service AND l.run = r.id
			WHERE r.service = $1 AND r.id = $2",
			&[&service, &run],
		)
		.await?
	else {
		return Ok(None);
	};
	let mut done = BTreeMap::new();
	let mut bytes = 0;
	let mut partial = false;
	for step in client
		.query(
			"SELECT name, coalesce(result, 'null'::jsonb), octet_length(coalesce(result, 'null'::jsonb)::text)::bigint
			FROM steps WHERE service = $1 AND run = $2 AND status = 'done' ORDER BY started_at, name",
			&[&service, &run],
		)
		.await?
	{
		let size: i64 = step.get(2);
		if bytes + size > TURN_BYTES {
			partial = true;
			continue;
		}
		bytes += size;
		done.insert(step.get::<_, String>(0), step.get::<_, Value>(1));
	}
	Ok(Some(Turn {
		service: service.to_owned(),
		run: run.to_owned(),
		workflow: row.get(0),
		params: row.get(1),
		fence: row.get(2),
		expires_at: timestamp(row.get(3)),
		done,
		partial,
	}))
}

/// A step's row as the coordinator reads it to decide.
struct Step {
	kind: String,
	status: String,
	attempt: i32,
	starts: i32,
	config: Value,
	result: Option<Value>,
	error: Option<String>,
	deadline: Option<i64>,
	wake_at: Option<i64>,
}

async fn step_of(
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

fn stage(name: &str) -> String {
	format!("step.{name}")
}

/// The run goes to `status` until `run_at`, and its lease is let go.
async fn release(
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

async fn fail_step(
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

fn millis(ms: u64) -> i64 {
	i64::try_from(ms).unwrap_or(i64::MAX).saturating_mul(1_000_000)
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

	/// One round of the scheduler, on the primary: leases that ran out are let go and their runs made
	/// ready, sleeps and waits that are due are made ready, and ready runs are leased to the
	/// executors heard from lately, the least loaded first. Confirmed as one commit. Answers the
	/// turns just granted, for the caller to push. Does nothing on a standby.
	pub async fn tick(&self) -> anyhow::Result<Vec<Granted>> {
		let mut client = self.pool.get().await?;
		let tx = client.transaction().await?;
		if tx.query_one("SELECT pg_is_in_recovery()", &[]).await?.get::<_, bool>(0) {
			return Ok(Vec::new());
		}
		let now = Self::clock(&tx).await?;
		tx.execute(
			"WITH gone AS (DELETE FROM leases WHERE expires_at < $1 RETURNING service, run)
			UPDATE runs r SET status = 'ready', run_at = $1, updated_at = $1 FROM gone
			WHERE r.service = gone.service AND r.id = gone.run AND r.status = 'running'",
			&[&now],
		)
		.await?;
		tx.execute(
			"UPDATE runs SET status = 'ready', updated_at = $1
			WHERE status IN ('sleeping', 'waiting') AND run_at <= $1",
			&[&now],
		)
		.await?;
		let executors: Vec<(String, i64, Option<String>)> = tx
			.query(
				"SELECT e.node, e.capacity - count(l.run), e.push_url
				FROM executors e LEFT JOIN leases l ON l.holder = e.node
				WHERE e.seen_at > $1 GROUP BY e.node, e.capacity, e.push_url
				HAVING e.capacity - count(l.run) > 0 ORDER BY 2 DESC, 1",
				&[&(now - ALIVE_NANOS)],
			)
			.await?
			.into_iter()
			.map(|row| (row.get(0), row.get(1), row.get(2)))
			.collect();
		let urls: std::collections::HashMap<String, Option<String>> =
			executors.iter().map(|(node, _, url)| (node.clone(), url.clone())).collect();
		let mut executors: Vec<(String, i64)> =
			executors.into_iter().map(|(node, free, _)| (node, free)).collect();
		let free: i64 = executors.iter().map(|(_, free)| free).sum();
		if free == 0 {
			tx.commit().await?;
			return Ok(Vec::new());
		}
		let ready = tx
			.query(
				"SELECT service, id FROM runs WHERE status = 'ready' AND run_at <= $1 AND class = 'quorum'
				ORDER BY run_at, service, id LIMIT $2 FOR UPDATE SKIP LOCKED",
				&[&now, &free],
			)
			.await?;
		let (mut services, mut ids, mut nodes) = (Vec::new(), Vec::new(), Vec::new());
		for run in &ready {
			// The node with the most room left takes the next run.
			executors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
			let taker = &mut executors[0];
			taker.1 -= 1;
			services.push(run.get::<_, String>(0));
			ids.push(run.get::<_, String>(1));
			nodes.push(taker.0.clone());
		}
		if ids.is_empty() {
			tx.commit().await?;
			return Ok(Vec::new());
		}
		let granted = tx
			.query(
				"UPDATE runs r SET status = 'running', fence = r.fence + 1, updated_at = $4
				FROM unnest($1::text[], $2::text[], $3::text[]) AS g(service, id, node)
				WHERE r.service = g.service AND r.id = g.id AND r.status = 'ready'
				RETURNING r.service, r.id, r.fence, g.node",
				&[&services, &ids, &nodes, &now],
			)
			.await?;
		let (mut services, mut ids, mut fences, mut nodes) =
			(Vec::new(), Vec::new(), Vec::new(), Vec::new());
		for run in &granted {
			services.push(run.get::<_, String>(0));
			ids.push(run.get::<_, String>(1));
			fences.push(run.get::<_, i64>(2));
			nodes.push(run.get::<_, String>(3));
		}
		tx.execute(
			"INSERT INTO leases (service, run, holder, fence, expires_at, granted_at)
			SELECT g.service, g.run, g.holder, g.fence, $5::bigint + $6::bigint, $5::bigint
			FROM unnest($1::text[], $2::text[], $3::text[], $4::bigint[]) AS g(service, run, holder, fence)
			ON CONFLICT (service, run) DO UPDATE SET holder = excluded.holder, fence = excluded.fence,
				expires_at = excluded.expires_at, granted_at = excluded.granted_at",
			&[&services, &ids, &nodes, &fences, &now, &LEASE_NANOS],
		)
		.await?;
		for index in 0..ids.len() {
			let (service, id) = (&services[index], &ids[index]);
			if get_in(&tx, service, id).await?.is_some_and(|task| task.task.record.state == State::Queued)
			{
				set_state(&tx, service, id, State::Running, now, None).await?;
				append(&tx, service, id, now, "running", Level::Info, "running", Value::Null).await?;
			}
			let data = serde_json::json!({ "node": nodes[index], "fence": fences[index] });
			append(&tx, service, id, now, "lease", Level::Info, "leased", data).await?;
		}
		tx.commit().await?;
		let client = self.pool.get().await?;
		let mut out = Vec::new();
		for index in 0..ids.len() {
			let Some(turn) = turn_for(&client, &services[index], &ids[index]).await? else { continue };
			let push_url = urls.get(&nodes[index]).cloned().flatten();
			out.push(Granted { node: nodes[index].clone(), push_url, turn });
		}
		Ok(out)
	}
}

#[cfg(test)]
mod tests;
