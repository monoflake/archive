//! Granting turns: an executor's poll and renewal, the turn it is handed, the scheduler's round.

use super::record::{append, set_state};
use super::{ALIVE_NANOS, LEASE_NANOS, Lease, NOW, TURN_BYTES, timestamp};
use crate::store::{LOCALLY, Store, get_in};
use deadpool_postgres::GenericClient;
use jiff::Timestamp;
use ledger::{Level, State};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// A turn handed to a node: the run to replay, the fence every write must carry, and what its done
/// steps returned, so the replay answers them without asking. `asked_at` is when the run was
/// opened, the same in every turn, which Workflows gives the code as `event.timestamp`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Turn {
	pub service: String,
	pub run: String,
	pub workflow: String,
	pub params: Value,
	pub asked_at: Timestamp,
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

impl Store {
	/// An executor's heartbeat, which also names where to push a turn to and the workflows it can
	/// run, and the turns it holds now. The heartbeat commits `locally`: a lost one only makes a node
	/// look absent for a moment.
	pub async fn poll(
		&self,
		node: &str,
		capacity: i32,
		push_url: Option<&str>,
		workflows: &[String],
	) -> anyhow::Result<Vec<Turn>> {
		let client = self.pool.get().await?;
		client
			.execute(
				&format!(
					"WITH {} INSERT INTO executors (node, seen_at, capacity, push_url, workflows) \
					 SELECT $1, {NOW}, $2, $3, $4 FROM locally \
					 ON CONFLICT (node) DO UPDATE SET seen_at = excluded.seen_at, \
					 capacity = excluded.capacity, push_url = excluded.push_url, \
					 workflows = excluded.workflows",
					LOCALLY
				),
				&[&node, &capacity, &push_url, &workflows],
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
					LOCALLY
				),
				&[&service, &run, &node, &fence, &LEASE_NANOS],
			)
			.await?;
		Ok(row.map(|row| timestamp(row.get(0))))
	}

	/// One round of the scheduler, on the primary: leases that ran out are let go and their runs made
	/// ready, sleeps and waits that are due are made ready, and ready runs are leased to the
	/// executors heard from lately that name their workflow, the least loaded first. Confirmed as
	/// one commit. Answers the turns just granted, for the caller to push. Does nothing on a standby.
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
		let executors: Vec<(String, i64, Option<String>, Vec<String>)> = tx
			.query(
				"SELECT e.node, e.capacity - count(l.run), e.push_url, e.workflows
				FROM executors e LEFT JOIN leases l ON l.holder = e.node
				WHERE e.seen_at > $1 GROUP BY e.node, e.capacity, e.push_url, e.workflows
				HAVING e.capacity - count(l.run) > 0 ORDER BY 2 DESC, 1",
				&[&(now - ALIVE_NANOS)],
			)
			.await?
			.into_iter()
			.map(|row| (row.get(0), row.get(1), row.get(2), row.get(3)))
			.collect();
		let urls: std::collections::HashMap<String, Option<String>> =
			executors.iter().map(|(node, _, url, _)| (node.clone(), url.clone())).collect();
		let mut executors: Vec<(String, i64, Vec<String>)> =
			executors.into_iter().map(|(node, free, _, names)| (node, free, names)).collect();
		let free: i64 = executors.iter().map(|(_, free, _)| free).sum();
		// Only a run some executor with room can run is taken, so one nobody names never holds a
		// place in the round that a run somebody does name could have had.
		let named: Vec<String> = executors
			.iter()
			.flat_map(|(_, _, names)| names.iter().cloned())
			.collect::<std::collections::BTreeSet<_>>()
			.into_iter()
			.collect();
		if free == 0 {
			tx.commit().await?;
			return Ok(Vec::new());
		}
		let ready = tx
			.query(
				"SELECT service, id, workflow FROM runs WHERE status = 'ready' AND run_at <= $1
				AND class = 'quorum' AND workflow = ANY($3)
				ORDER BY run_at, service, id LIMIT $2 FOR UPDATE SKIP LOCKED",
				&[&now, &free, &named],
			)
			.await?;
		let (mut services, mut ids, mut nodes) = (Vec::new(), Vec::new(), Vec::new());
		for run in &ready {
			// The node with the most room left of those naming the run's workflow takes it; a run none
			// of them has room for waits for the next round.
			let workflow: String = run.get(2);
			executors.sort_by(|a, b| b.1.cmp(&a.1).then_with(|| a.0.cmp(&b.0)));
			let Some(taker) =
				executors.iter_mut().find(|(_, free, names)| *free > 0 && names.contains(&workflow))
			else {
				continue;
			};
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

/// The turn the run's lease holds, with what its done steps returned.
async fn turn_for(
	client: &impl GenericClient,
	service: &str,
	run: &str,
) -> anyhow::Result<Option<Turn>> {
	let Some(row) = client
		.query_opt(
			"SELECT r.workflow, r.params, l.fence, l.expires_at, r.created_at
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
		asked_at: timestamp(row.get(4)),
		fence: row.get(2),
		expires_at: timestamp(row.get(3)),
		done,
		partial,
	}))
}
