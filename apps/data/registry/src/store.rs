//! The nodes and the apps, each keyed by name, and every change made to them, appended and never
//! rewritten. See spec/architecture/registry.md, RFC ja5hm and RFC cpc2b.

use crate::kinds::Kind;
use crate::migrations;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use serde::Serialize;
use serde_json::Value;
use tokio_postgres::{IsolationLevel, NoTls};

/// How many connections one registry holds at most: every node's relay and host read, an operator
/// and the hook write.
const CONNECTIONS: usize = 4;

/// The lock every write holds until it commits, so a write's revision is checked and its change
/// appended as one step, and changes are numbered in the order they become visible: "nodes" in
/// ASCII.
const WRITES: i64 = 0x6e_6f64_6573;

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

	/// Sets `name` of `kind` to `spec` if it is still at `revision`, appending the change. One that
	/// does not exist is at 0, even where changes to it are kept from before the old `DELETE /nodes`
	/// removed it. See RFC cpc2b.
	pub async fn set(
		&self,
		kind: Kind,
		name: &str,
		by: &str,
		revision: i64,
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
		if revision != kept_at {
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
		let id = append(&transaction, kind, by, name, before, spec).await?;
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
}

async fn append(
	transaction: &deadpool_postgres::Transaction<'_>,
	kind: Kind,
	by: &str,
	name: &str,
	before: Option<Value>,
	after: Value,
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
