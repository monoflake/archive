//! The nodes and the apps, each keyed by name, and every change made to them, appended and never
//! rewritten. See spec/architecture/registry.md, RFC ja5hm and RFC cpc2b.

use crate::kinds::Kind;
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

/// Every node, by name, and the newest change made to a node. What `GET /nodes` answers.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Nodes {
	pub nodes: Map<String, Value>,
	pub change: i64,
}

/// One node's row of `changes`: who set `name` from `before` to `after`, and when. A node created
/// has no `before`, and one deleted no `after`. What `GET /changes` answers.
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

/// One resource as `/v1` answers it. Its `status` and `drift` are absent until a controller reports
/// one -- RFC jln34.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Resource {
	pub kind: Kind,
	pub name: String,
	/// The id of the newest change made to it.
	pub revision: i64,
	pub spec: Value,
}

/// Every resource of a kind, by name, and the newest change made to anything.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Listed {
	pub items: Vec<Resource>,
	pub revision: i64,
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Set {
	/// Nothing: the resource changed after the revision given.
	Stale,
	/// Nothing: the spec kept is already equal to it.
	Unchanged,
	/// The change it appended.
	Changed(i64),
}

/// The table a kind is kept in, and the column holding its spec.
const fn table(kind: Kind) -> (&'static str, &'static str) {
	match kind {
		Kind::Node => ("nodes", "record"),
		Kind::App => ("apps", "spec"),
	}
}

/// A resource's revision, 0 where it has no change, in a query over its kind's table as `r`.
const REVISION: &str =
	"coalesce((SELECT max(c.id) FROM changes c WHERE c.kind = $1 AND c.name = r.name), 0)";

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
		let change: i64 = transaction
			.query_one("SELECT coalesce(max(id), 0) FROM changes WHERE kind = 'node'", &[])
			.await?
			.get(0);
		transaction.commit().await?;
		let nodes = rows.into_iter().map(|row| (row.get(0), row.get(1))).collect();
		Ok(Nodes { nodes, change })
	}

	/// Sets node `name` to `record` and appends the change, answering its id; `None`, with nothing
	/// written, when the record kept is already equal to it.
	pub async fn put(&self, name: &str, by: &str, record: Value) -> anyhow::Result<Option<i64>> {
		match self.write(Kind::Node, name, by, None, record).await? {
			Set::Changed(id) => Ok(Some(id)),
			Set::Stale | Set::Unchanged => Ok(None),
		}
	}

	/// Sets `name` of `kind` to `spec` if it is still at `revision`, 0 for one that does not exist,
	/// appending the change. See RFC cpc2b.
	pub async fn set(
		&self,
		kind: Kind,
		name: &str,
		by: &str,
		revision: i64,
		spec: Value,
	) -> anyhow::Result<Set> {
		self.write(kind, name, by, Some(revision), spec).await
	}

	/// A write, held to `revision` where one is given: one that does not exist is at 0, whatever
	/// changes were made to it before it was deleted.
	async fn write(
		&self,
		kind: Kind,
		name: &str,
		by: &str,
		revision: Option<i64>,
		spec: Value,
	) -> anyhow::Result<Set> {
		let (table, column) = table(kind);
		let mut client = self.pool.get().await?;
		let transaction = client.transaction().await?;
		transaction.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITES]).await?;
		let kept = transaction
			.query_opt(
				&format!("SELECT {column}, {column} = $3, {REVISION} FROM {table} r WHERE name = $2"),
				&[&kind.name(), &name, &spec],
			)
			.await?;
		let kept_at = kept.as_ref().map_or(0, |row| row.get::<_, i64>(2));
		if revision.is_some_and(|revision| revision != kept_at) {
			return Ok(Set::Stale);
		}
		let before: Option<Value> = match kept {
			Some(row) if row.get::<_, bool>(1) => return Ok(Set::Unchanged),
			Some(row) => Some(row.get(0)),
			None => None,
		};
		transaction
			.execute(
				&format!(
					"INSERT INTO {table} (name, {column}) VALUES ($1, $2)
					ON CONFLICT (name) DO UPDATE SET {column} = excluded.{column}, updated_at = now()"
				),
				&[&name, &spec],
			)
			.await?;
		let id = append(&transaction, kind, by, name, before, Some(spec)).await?;
		transaction.commit().await?;
		Ok(Set::Changed(id))
	}

	/// Every resource of `kind` in name order, and the newest change to anything, read from one
	/// snapshot so the two agree.
	pub async fn resources(&self, kind: Kind) -> anyhow::Result<Listed> {
		let (table, column) = table(kind);
		let mut client = self.pool.get().await?;
		let transaction = client
			.build_transaction()
			.isolation_level(IsolationLevel::RepeatableRead)
			.read_only(true)
			.start()
			.await?;
		let rows = transaction
			.query(
				&format!("SELECT r.name, r.{column}, {REVISION} FROM {table} r ORDER BY r.name"),
				&[&kind.name()],
			)
			.await?;
		let revision: i64 =
			transaction.query_one("SELECT coalesce(max(id), 0) FROM changes", &[]).await?.get(0);
		transaction.commit().await?;
		let items = rows
			.into_iter()
			.map(|row| Resource { kind, name: row.get(0), revision: row.get(2), spec: row.get(1) })
			.collect();
		Ok(Listed { items, revision })
	}

	/// The resource `name` of `kind`, if there is one.
	pub async fn resource(&self, kind: Kind, name: &str) -> anyhow::Result<Option<Resource>> {
		let (table, column) = table(kind);
		let row = self
			.pool
			.get()
			.await?
			.query_opt(
				&format!("SELECT r.{column}, {REVISION} FROM {table} r WHERE r.name = $2"),
				&[&kind.name(), &name],
			)
			.await?;
		Ok(row.map(|row| Resource { kind, name: name.into(), revision: row.get(1), spec: row.get(0) }))
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
		let id = append(&transaction, Kind::Node, by, name, Some(row.get(0)), None).await?;
		transaction.commit().await?;
		Ok(Some(id))
	}

	/// Oldest first, at most `limit` changes to nodes, only those numbered after `after`.
	pub async fn changes(&self, after: i64, limit: usize) -> anyhow::Result<Vec<Change>> {
		let rows = self
			.pool
			.get()
			.await?
			.query(
				"SELECT id, at, by, name, before, after FROM changes
				WHERE id > $1 AND kind = 'node' ORDER BY id LIMIT $2",
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
	kind: Kind,
	by: &str,
	name: &str,
	before: Option<Value>,
	after: Option<Value>,
) -> anyhow::Result<i64> {
	let row = transaction
		.query_one(
			"INSERT INTO changes (kind, by, name, before, after) VALUES ($1, $2, $3, $4, $5)
			RETURNING id",
			&[&kind.name(), &by, &name, &before, &after],
		)
		.await?;
	Ok(row.get(0))
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
