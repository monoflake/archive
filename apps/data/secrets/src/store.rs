//! Every secret's ciphertext, keyed by scope, app and name, and every change made to one, appended
//! and never rewritten: who, when and which, never the value. See RFC 231yq.

use crate::envelope::{Identity, Sealed};
use crate::migrations;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use serde::Serialize;
use tokio_postgres::{IsolationLevel, NoTls};

/// How many connections one copy holds at most: every node reads now and then, an operator writes.
const CONNECTIONS: usize = 4;

/// The lock every write holds until it commits, so changes are numbered in the order they become
/// visible: "changes" in ASCII.
const WRITES: i64 = 0x63_6861_6e67_6573;

/// A secret's revision, 0 where it has no change, in a query over `secrets` as `s`.
const REVISION: &str = "coalesce((SELECT max(c.id) FROM changes c
	WHERE c.scope = s.scope AND c.app = s.app AND c.name = s.name), 0)";

/// A secret as `GET /v1/secrets` lists it: what it is and its revision, never its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Named {
	pub scope: String,
	pub app: String,
	pub name: String,
	/// The id of the newest change made to it.
	pub revision: i64,
}

/// Every secret by scope, app and name, and the newest change made to any.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Listed {
	pub items: Vec<Named>,
	pub revision: i64,
}

/// A secret as it is kept, still sealed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Kept {
	pub named: Named,
	pub sealed: Sealed,
}

impl Kept {
	pub fn identity(&self) -> Identity<'_> {
		Identity { scope: &self.named.scope, app: &self.named.app, name: &self.named.name }
	}
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Set {
	/// Nothing: the secret changed after the revision given.
	Stale,
	/// Nothing: the value kept is already equal to it.
	Unchanged,
	/// The change it appended.
	Changed(i64),
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

	/// Every secret in order, and the newest change, read from one snapshot so the two agree.
	pub async fn list(&self) -> anyhow::Result<Listed> {
		let mut client = self.pool.get().await?;
		let transaction = client
			.build_transaction()
			.isolation_level(IsolationLevel::RepeatableRead)
			.read_only(true)
			.start()
			.await?;
		let rows = transaction
			.query(
				&format!("SELECT s.scope, s.app, s.name, {REVISION} FROM secrets s ORDER BY 1, 2, 3"),
				&[],
			)
			.await?;
		let revision: i64 =
			transaction.query_one("SELECT coalesce(max(id), 0) FROM changes", &[]).await?.get(0);
		transaction.commit().await?;
		let items = rows
			.into_iter()
			.map(|row| Named {
				scope: row.get(0),
				app: row.get(1),
				name: row.get(2),
				revision: row.get(3),
			})
			.collect();
		Ok(Listed { items, revision })
	}

	/// Every secret of `apps`, in every scope, still sealed, and the newest change to anything.
	pub async fn of_apps(&self, apps: &[String]) -> anyhow::Result<(Vec<Kept>, i64)> {
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
					"SELECT s.scope, s.app, s.name, {REVISION}, s.data_key, s.value FROM secrets s
					WHERE s.app = ANY($1) ORDER BY 1, 2, 3"
				),
				&[&apps],
			)
			.await?;
		let revision: i64 =
			transaction.query_one("SELECT coalesce(max(id), 0) FROM changes", &[]).await?.get(0);
		transaction.commit().await?;
		Ok((rows.into_iter().map(|row| kept(&row)).collect(), revision))
	}

	/// Any one secret, for checking a key the operator gives where there is no share to check it by.
	pub async fn any(&self) -> anyhow::Result<Option<Kept>> {
		let row = self
			.pool
			.get()
			.await?
			.query_opt(
				&format!(
					"SELECT s.scope, s.app, s.name, {REVISION}, s.data_key, s.value FROM secrets s LIMIT 1"
				),
				&[],
			)
			.await?;
		Ok(row.as_ref().map(kept))
	}

	/// Sets `identity` to `sealed` if it is still at `revision`, 0 for one that does not exist,
	/// appending the change. `same` says whether what is kept already holds the value being written,
	/// which only the root key can tell, so a write of the value kept writes nothing.
	pub async fn set(
		&self,
		identity: &Identity<'_>,
		by: &str,
		revision: i64,
		sealed: &Sealed,
		same: impl FnOnce(&Sealed) -> bool,
	) -> anyhow::Result<Set> {
		let Identity { scope, app, name } = *identity;
		let mut client = self.pool.get().await?;
		let transaction = client.transaction().await?;
		transaction.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITES]).await?;
		let kept = transaction
			.query_opt(
				&format!(
					"SELECT s.data_key, s.value, {REVISION} FROM secrets s
					WHERE s.scope = $1 AND s.app = $2 AND s.name = $3"
				),
				&[&scope, &app, &name],
			)
			.await?;
		let kept_at = kept.as_ref().map_or(0, |row| row.get::<_, i64>(2));
		if revision != kept_at {
			return Ok(Set::Stale);
		}
		if let Some(row) = kept
			&& same(&Sealed { data_key: row.get(0), value: row.get(1) })
		{
			return Ok(Set::Unchanged);
		}
		transaction
			.execute(
				"INSERT INTO secrets (scope, app, name, data_key, value) VALUES ($1, $2, $3, $4, $5)
				ON CONFLICT (scope, app, name) DO UPDATE
				SET data_key = excluded.data_key, value = excluded.value, updated_at = now()",
				&[&scope, &app, &name, &sealed.data_key, &sealed.value],
			)
			.await?;
		let id: i64 = transaction
			.query_one(
				"INSERT INTO changes (by, scope, app, name) VALUES ($1, $2, $3, $4) RETURNING id",
				&[&by, &scope, &app, &name],
			)
			.await?
			.get(0);
		transaction.commit().await?;
		Ok(Set::Changed(id))
	}
}

fn kept(row: &tokio_postgres::Row) -> Kept {
	Kept {
		named: Named { scope: row.get(0), app: row.get(1), name: row.get(2), revision: row.get(3) },
		sealed: Sealed { data_key: row.get(4), value: row.get(5) },
	}
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
