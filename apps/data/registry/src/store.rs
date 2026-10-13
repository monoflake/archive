//! The nodes, keyed by name, and every change made to them, appended and never rewritten. See
//! spec/architecture/registry.md and RFC ja5hm.

use crate::migrations;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::{Map, Value};
use tokio_postgres::{IsolationLevel, NoTls};

/// How many connections one registry holds at most: every node's relay reads, an operator writes.
const CONNECTIONS: usize = 4;

/// The lock every write holds until it commits, so changes are numbered in the order they become
/// visible and a reader following `after` never passes one still being written: "nodes" in ASCII.
const WRITES: i64 = 0x6e_6f64_6573;

/// Every node, by name, and the newest change they reflect. What `GET /nodes` answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Nodes {
	pub nodes: Map<String, Value>,
	pub change: i64,
}

/// One row of `changes`: who set `name` from `before` to `after`, and when. A node created has no
/// `before`, and one deleted no `after`.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Change {
	pub id: i64,
	pub at: Timestamp,
	pub by: String,
	pub name: String,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub before: Option<Value>,
	#[serde(skip_serializing_if = "Option::is_none")]
	pub after: Option<Value>,
}

pub struct Store {
	config: tokio_postgres::Config,
	pool: Pool,
}

impl Store {
	/// A pool over `url`, `DATABASE_URL` as `mise run database grant` writes it; nothing is connected
	/// until something is asked. `schema`, in tests alone, is where every table is made and read.
	pub fn connect(url: &str, schema: Option<&str>) -> anyhow::Result<Self> {
		let mut config: tokio_postgres::Config = url.parse()?;
		if let Some(schema) = schema {
			config.options(format!("-c search_path={schema}"));
		}
		let manager = Manager::from_config(
			config.clone(),
			NoTls,
			ManagerConfig { recycling_method: RecyclingMethod::Fast },
		);
		let pool = Pool::builder(manager).max_size(CONNECTIONS).build()?;
		Ok(Self { config, pool })
	}

	/// The schema brought up to date, on a connection of its own. See migrations.rs.
	pub async fn migrate(&self) -> anyhow::Result<Vec<i32>> {
		let (mut client, connection) = self.config.connect(NoTls).await?;
		let driven = tokio::spawn(connection);
		let applied = migrations::run(&mut client).await;
		drop(client);
		let _ = driven.await;
		Ok(applied?)
	}

	/// Whether the database answers, for `/health`.
	pub async fn answers(&self) -> anyhow::Result<()> {
		self.pool.get().await?.execute("SELECT 1", &[]).await?;
		Ok(())
	}

	/// Every node in name order, and the newest change, read from one snapshot so the two agree.
	pub async fn nodes(&self) -> anyhow::Result<Nodes> {
		let mut client = self.pool.get().await?;
		let transaction = client
			.build_transaction()
			.isolation_level(IsolationLevel::RepeatableRead)
			.read_only(true)
			.start()
			.await?;
		let rows = transaction.query("SELECT name, record FROM nodes ORDER BY name", &[]).await?;
		let change: i64 =
			transaction.query_one("SELECT coalesce(max(id), 0) FROM changes", &[]).await?.get(0);
		transaction.commit().await?;
		let nodes = rows.into_iter().map(|row| (row.get(0), row.get(1))).collect();
		Ok(Nodes { nodes, change })
	}

	/// Sets `name` to `record` and appends the change, answering its id; `None`, with nothing
	/// written, when the record kept is already equal to it.
	pub async fn put(&self, name: &str, by: &str, record: Value) -> anyhow::Result<Option<i64>> {
		let mut client = self.pool.get().await?;
		let transaction = client.transaction().await?;
		transaction.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITES]).await?;
		let kept = transaction
			.query_opt("SELECT record, record = $2 FROM nodes WHERE name = $1", &[&name, &record])
			.await?;
		let before: Option<Value> = match kept {
			Some(row) if row.get::<_, bool>(1) => return Ok(None),
			Some(row) => Some(row.get(0)),
			None => None,
		};
		transaction
			.execute(
				"INSERT INTO nodes (name, record) VALUES ($1, $2)
				ON CONFLICT (name) DO UPDATE SET record = excluded.record, updated_at = now()",
				&[&name, &record],
			)
			.await?;
		let id = append(&transaction, by, name, before, Some(record)).await?;
		transaction.commit().await?;
		Ok(Some(id))
	}

	/// Deletes `name` and appends the change, answering its id; `None` when there is no such node.
	pub async fn delete(&self, name: &str, by: &str) -> anyhow::Result<Option<i64>> {
		let mut client = self.pool.get().await?;
		let transaction = client.transaction().await?;
		transaction.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITES]).await?;
		let Some(row) =
			transaction.query_opt("DELETE FROM nodes WHERE name = $1 RETURNING record", &[&name]).await?
		else {
			return Ok(None);
		};
		let id = append(&transaction, by, name, Some(row.get(0)), None).await?;
		transaction.commit().await?;
		Ok(Some(id))
	}

	/// Oldest first, at most `limit` changes, only those numbered after `after`.
	pub async fn changes(&self, after: i64, limit: usize) -> anyhow::Result<Vec<Change>> {
		let rows = self
			.pool
			.get()
			.await?
			.query(
				"SELECT id, at, by, name, before, after FROM changes WHERE id > $1 ORDER BY id LIMIT $2",
				&[&after, &i64::try_from(limit)?],
			)
			.await?;
		Ok(
			rows
				.into_iter()
				.map(|row| Change {
					id: row.get(0),
					at: row.get(1),
					by: row.get(2),
					name: row.get(3),
					before: row.get(4),
					after: row.get(5),
				})
				.collect(),
		)
	}
}

async fn append(
	transaction: &deadpool_postgres::Transaction<'_>,
	by: &str,
	name: &str,
	before: Option<Value>,
	after: Option<Value>,
) -> anyhow::Result<i64> {
	let row = transaction
		.query_one(
			"INSERT INTO changes (by, name, before, after) VALUES ($1, $2, $3, $4) RETURNING id",
			&[&by, &name, &before, &after],
		)
		.await?;
	Ok(row.get(0))
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
