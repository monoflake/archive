//! The nodes, the apps and the accounts, each kept by id and the named ones found by name too, and
//! every change made to them, appended and never rewritten. See spec/architecture/registry.md, RFC
//! ja5hm and RFC cpc2b.

use crate::kinds::{self, Kind};
use crate::migrations;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;
use std::convert::Infallible;
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
	/// 16 lowercase hexadecimal digits, made when it was first written and never changed -- RFC
	/// 2kuvi.
	pub id: String,
	/// None for a kind with no name, an account's -- RFC 32hd9.
	pub name: Option<String>,
	/// The id of the newest change made to it.
	pub revision: i64,
	pub spec: Value,
}

/// Every resource of a kind, by name then id, and the newest change made to anything.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Listed {
	pub items: Vec<Resource>,
	pub revision: i64,
}

/// What a write did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Set {
	/// Nothing: the resource changed after the revision given.
	Stale,
	/// Nothing: the spec kept is already equal to it.
	Unchanged,
	/// The change it appended.
	Changed(Written),
}

/// A change appended: its id, the resource's revision from now on; the resource's id and name; the
/// task it is told to the ledger as, whose id the history row keeps; and when it was made. Both ids
/// are 16 lowercase hexadecimal digits -- RFC 2kuvi.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Written {
	pub revision: i64,
	pub id: String,
	/// The resource's name, none for a kind with no name.
	pub name: Option<String>,
	pub task: String,
	pub at: Timestamp,
}

/// How a route names a resource: by its id when the key has an id's shape, by its name otherwise --
/// RFC 2kuvi.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key<'a> {
	Id(&'a str),
	Name(&'a str),
}

impl<'a> Key<'a> {
	pub fn of(key: &'a str) -> Self {
		if kinds::is_id(key) { Self::Id(key) } else { Self::Name(key) }
	}
}

/// What a write sets: a named kind's resource by its name, made where it is not held; one held, by
/// its id; or a new one of a kind with no name, its id drawn here.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Target<'a> {
	Named(&'a str),
	Held(&'a str),
	Unnamed,
}

/// The table a kind is kept in, and the column holding its spec.
const fn table(kind: Kind) -> (&'static str, &'static str) {
	match kind {
		Kind::Node => ("nodes", "record"),
		Kind::App => ("apps", "spec"),
		Kind::Account => ("accounts", "spec"),
	}
}

/// How many ids are drawn for one write before a collision is taken as a fault: 64 random bits
/// collide by chance about never, so a second collision says the source is broken, not unlucky.
const DRAWS: usize = 4;

/// A resource's revision, 0 where it has no change, in a query over its kind's table as `r`: its
/// newest change, by its id, which every change made to a resource held carries.
const REVISION: &str =
	"coalesce((SELECT max(c.id) FROM changes c WHERE c.kind = $1 AND c.resource = r.id), 0)";

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

	/// Sets the resource `target` names to `spec` if it is still at `revision`, appending the
	/// change. One that does not exist is at 0, and one held that `target` names by an id it does not
	/// hold is stale. See RFC cpc2b.
	pub async fn set(
		&self,
		kind: Kind,
		target: Target<'_>,
		by: &str,
		revision: i64,
		spec: Value,
	) -> anyhow::Result<Set> {
		let made = self.set_with(kind, target, by, revision, |_| async { Ok::<_, Infallible>(spec) });
		Ok(made.await?.unwrap_or_else(|never| match never {}))
	}

	/// `set`, with the spec made by `make` once the revision holds and the resource's id is known --
	/// the one it holds, or the one drawn for it -- under the lock every write holds. A `make` that
	/// fails is answered as it failed, and nothing is written: how a value bound for the secret
	/// service would reach it under a name its id is in, before anything here changes.
	pub async fn set_with<F, Made, E>(
		&self,
		kind: Kind,
		target: Target<'_>,
		by: &str,
		revision: i64,
		make: F,
	) -> anyhow::Result<Result<Set, E>>
	where
		F: FnOnce(String) -> Made,
		Made: Future<Output = Result<Value, E>>,
	{
		let (table, column) = table(kind);
		let mut client = self.pool.get().await?;
		let transaction = client.transaction().await?;
		transaction.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITES]).await?;
		let read = format!("SELECT {column}, id, name, {REVISION} FROM {table} r");
		let kept = match target {
			Target::Named(name) => {
				transaction.query_opt(&format!("{read} WHERE name = $2"), &[&kind.name(), &name]).await?
			}
			Target::Held(id) => {
				let held =
					transaction.query_opt(&format!("{read} WHERE id = $2"), &[&kind.name(), &id]).await?;
				if held.is_none() {
					return Ok(Ok(Set::Stale));
				}
				held
			}
			Target::Unnamed => None,
		};
		let kept_at = kept.as_ref().map_or(0, |row| row.get::<_, i64>(3));
		if revision != kept_at {
			return Ok(Ok(Set::Stale));
		}
		// An id is made by the write that first holds the resource, and an update never names it,
		// so it never changes; nor does a name, which only a named kind's first write gives.
		let (before, id, name): (Option<Value>, String, Option<String>) = match (kept, target) {
			(Some(row), _) => (Some(row.get(0)), row.get(1), row.get(2)),
			(None, Target::Named(name)) => {
				(None, unused(&transaction, table, "id").await?, Some(name.to_owned()))
			}
			(None, _) => (None, unused(&transaction, table, "id").await?, None),
		};
		let spec = match make(id.clone()).await {
			Ok(spec) => spec,
			Err(refusal) => return Ok(Err(refusal)),
		};
		if let Some(before) = &before {
			let same: bool =
				transaction.query_one("SELECT $1::jsonb = $2::jsonb", &[before, &spec]).await?.get(0);
			if same {
				return Ok(Ok(Set::Unchanged));
			}
			transaction
				.execute(
					&format!("UPDATE {table} SET {column} = $2, updated_at = now() WHERE id = $1"),
					&[&id, &spec],
				)
				.await?;
		} else {
			transaction
				.execute(
					&format!("INSERT INTO {table} (id, name, {column}) VALUES ($1, $2, $3)"),
					&[&id, &name, &spec],
				)
				.await?;
		}
		let task = unused(&transaction, "changes", "task").await?;
		let row = transaction
			.query_one(
				"INSERT INTO changes (kind, by, name, before, after, resource, task)
				VALUES ($1, $2, $3, $4, $5, $6, $7) RETURNING id, at",
				&[&kind.name(), &by, &name, &before, &spec, &id, &task],
			)
			.await?;
		transaction.commit().await?;
		let written = Written { revision: row.get(0), id, name, task, at: row.get(1) };
		Ok(Ok(Set::Changed(written)))
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
				&format!(
					"SELECT r.id, r.name, r.{column}, {REVISION} FROM {table} r ORDER BY r.name, r.id"
				),
				&[&kind.name()],
			)
			.await?;
		let revision: i64 =
			transaction.query_one("SELECT coalesce(max(id), 0) FROM changes", &[]).await?.get(0);
		transaction.commit().await?;
		let items = rows.into_iter().map(|row| resource(kind, &row)).collect();
		Ok(Listed { items, revision })
	}

	/// The resource of `kind` that `key` names, if there is one.
	pub async fn resource(&self, kind: Kind, key: Key<'_>) -> anyhow::Result<Option<Resource>> {
		let (table, column) = table(kind);
		let (by, value) = match key {
			Key::Id(id) => ("id", id),
			Key::Name(name) => ("name", name),
		};
		let row = self
			.pool
			.get()
			.await?
			.query_opt(
				&format!("SELECT r.id, r.name, r.{column}, {REVISION} FROM {table} r WHERE r.{by} = $2"),
				&[&kind.name(), &value],
			)
			.await?;
		Ok(row.map(|row| resource(kind, &row)))
	}
}

/// An id `column` of `table` does not hold yet, drawn from the migration's `registry_id()`, and
/// drawn again past a collision at most `DRAWS` times. The caller holds `WRITES`, so no other write
/// of this release takes the same id between the check and the insert.
async fn unused(
	transaction: &deadpool_postgres::Transaction<'_>,
	table: &str,
	column: &str,
) -> anyhow::Result<String> {
	for _ in 0..DRAWS {
		let id: String = transaction.query_one("SELECT registry_id()", &[]).await?.get(0);
		let taken = format!("SELECT EXISTS (SELECT 1 FROM {table} WHERE {column} = $1)");
		if !transaction.query_one(&taken, &[&id]).await?.get::<_, bool>(0) {
			return Ok(id);
		}
	}
	anyhow::bail!("{DRAWS} ids drawn for {table}.{column} were each taken already")
}

/// A row selected as `id, name, spec, revision`.
fn resource(kind: Kind, row: &tokio_postgres::Row) -> Resource {
	Resource { kind, id: row.get(0), name: row.get(1), spec: row.get(2), revision: row.get(3) }
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
