//! Every value's ciphertext, keyed by scope, app and name, with its class, and every change made to
//! one and every comparison asked of one, appended and never rewritten: who, when and which, never
//! the value. See RFC 231yq and spec/architecture/secrets.md.

use crate::envelope::{Identity, Sealed};
use crate::migrations;
use deadpool_postgres::{Manager, ManagerConfig, Pool, RecyclingMethod};
use jiff::Timestamp;
use serde::{Deserialize, Serialize};
use std::time::Duration;
use tokio_postgres::{IsolationLevel, NoTls};

/// How many connections one copy holds at most: every node reads now and then, an operator writes.
const CONNECTIONS: usize = 4;

/// The lock every write holds until it commits, so changes are numbered in the order they become
/// visible: "changes" in ASCII.
const WRITES: i64 = 0x63_6861_6e67_6573;

/// A value's revision, 0 where it has no change, in a query over `secrets` as `s`.
const REVISION: &str = "coalesce((SELECT max(c.id) FROM changes c
	WHERE c.scope = s.scope AND c.app = s.app AND c.name = s.name), 0)";

/// What every read of a value selects, in the order `kept` reads it.
const COLUMNS: &str = "s.scope, s.app, s.name, s.class, s.updated_at, s.data_key, s.value";

/// Whether a value is answered to those who may read its app's values, or to its node alone.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
	Config,
	Secret,
}

impl Class {
	pub fn name(self) -> &'static str {
		match self {
			Class::Config => "config",
			Class::Secret => "secret",
		}
	}

	fn from_name(name: &str) -> Self {
		if name == "config" { Class::Config } else { Class::Secret }
	}
}

/// A value as a list says it: what it is, its class, its revision and when it changed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Named {
	pub scope: String,
	pub app: String,
	pub name: String,
	pub class: Class,
	/// The id of the newest change made to it.
	pub revision: i64,
	pub changed: Timestamp,
}

/// A value as it is kept, still sealed.
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

/// What a write asks for: a value and its class, or a class alone for the value kept.
#[derive(Debug, Clone, Copy)]
pub enum Wanted<'a> {
	Value(&'a Sealed, Class),
	Class(Class),
}

/// What a write did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Set {
	/// Nothing: the value changed after the revision given.
	Stale,
	/// Nothing: the value and class kept are already what was asked.
	Unchanged,
	/// Nothing: a class alone was asked of a value that does not exist.
	Missing,
	/// Nothing: a class alone would have made a `secret` readable, which takes its value written.
	Revealing,
	/// The change it appended.
	Changed(i64),
}

/// Comparisons each secret answers in a window, and the window, counted across every copy. See
/// spec/architecture/secrets.md.
pub const COMPARISONS: i64 = 5;
pub const WINDOW: Duration = Duration::from_secs(60);

/// The lock a comparison holds while it counts and records: "compares" in ASCII.
const COMPARES: i64 = 0x636f_6d70_6172_6573;

/// What a comparison did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Compared {
	/// The row it appended.
	Kept(i64),
	/// Nothing: the secret has answered its share of the window, one more let through after this.
	Limited(Duration),
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

	/// Every value in order, of one app of one scope alone when `held` names them, still sealed, and
	/// the newest change among them, read from one snapshot so the two agree.
	pub async fn list(&self, held: Option<(&str, &str)>) -> anyhow::Result<(Vec<Kept>, i64)> {
		let (scope, app) = held.unzip();
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
					"SELECT {COLUMNS}, {REVISION} FROM secrets s
					WHERE ($1::text IS NULL OR s.scope = $1) AND ($2::text IS NULL OR s.app = $2)
					ORDER BY 1, 2, 3"
				),
				&[&scope, &app],
			)
			.await?;
		let revision: i64 = transaction
			.query_one(
				"SELECT coalesce(max(id), 0) FROM changes
				WHERE ($1::text IS NULL OR scope = $1) AND ($2::text IS NULL OR app = $2)",
				&[&scope, &app],
			)
			.await?
			.get(0);
		transaction.commit().await?;
		Ok((rows.iter().map(kept).collect(), revision))
	}

	/// Every value of `apps` in `scopes` alone, still sealed, and the newest change to anything.
	pub async fn of_apps(
		&self,
		apps: &[String],
		scopes: &[String],
	) -> anyhow::Result<(Vec<Kept>, i64)> {
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
					"SELECT {COLUMNS}, {REVISION} FROM secrets s
					WHERE s.app = ANY($1) AND s.scope = ANY($2) ORDER BY 1, 2, 3"
				),
				&[&apps, &scopes],
			)
			.await?;
		let revision: i64 =
			transaction.query_one("SELECT coalesce(max(id), 0) FROM changes", &[]).await?.get(0);
		transaction.commit().await?;
		Ok((rows.iter().map(kept).collect(), revision))
	}

	/// The value `identity` names, still sealed, if there is one.
	pub async fn get(&self, identity: &Identity<'_>) -> anyhow::Result<Option<Kept>> {
		let row = self
			.pool
			.get()
			.await?
			.query_opt(
				&format!(
					"SELECT {COLUMNS}, {REVISION} FROM secrets s
					WHERE s.scope = $1 AND s.app = $2 AND s.name = $3"
				),
				&[&identity.scope, &identity.app, &identity.name],
			)
			.await?;
		Ok(row.as_ref().map(kept))
	}

	/// Any one value, for checking a key the operator gives where there is no share to check it by.
	pub async fn any(&self) -> anyhow::Result<Option<Kept>> {
		let row = self
			.pool
			.get()
			.await?
			.query_opt(&format!("SELECT {COLUMNS}, {REVISION} FROM secrets s LIMIT 1"), &[])
			.await?;
		Ok(row.as_ref().map(kept))
	}

	/// Sets `identity` as `wanted` asks if it is still at `revision`, 0 for one that does not exist,
	/// appending the change with the class it set. `same` says whether what is kept already holds
	/// the value being written, which only the root key can tell, so a write of the value and class
	/// kept writes nothing. A class alone makes a `config` a `secret`, never the other way.
	pub async fn set(
		&self,
		identity: &Identity<'_>,
		by: &str,
		revision: i64,
		wanted: Wanted<'_>,
		same: impl FnOnce(&Sealed) -> bool,
	) -> anyhow::Result<Set> {
		let Identity { scope, app, name } = *identity;
		let mut client = self.pool.get().await?;
		let transaction = client.transaction().await?;
		transaction.execute("SELECT pg_advisory_xact_lock($1)", &[&WRITES]).await?;
		let kept = transaction
			.query_opt(
				&format!(
					"SELECT {COLUMNS}, {REVISION} FROM secrets s
					WHERE s.scope = $1 AND s.app = $2 AND s.name = $3"
				),
				&[&scope, &app, &name],
			)
			.await?
			.as_ref()
			.map(kept);
		if revision != kept.as_ref().map_or(0, |kept| kept.named.revision) {
			return Ok(Set::Stale);
		}
		let class = match wanted {
			Wanted::Value(sealed, class) => {
				if let Some(kept) = &kept
					&& kept.named.class == class
					&& same(&kept.sealed)
				{
					return Ok(Set::Unchanged);
				}
				transaction
					.execute(
						"INSERT INTO secrets (scope, app, name, data_key, value, class)
						VALUES ($1, $2, $3, $4, $5, $6)
						ON CONFLICT (scope, app, name) DO UPDATE SET data_key = excluded.data_key,
							value = excluded.value, class = excluded.class, updated_at = now()",
						&[&scope, &app, &name, &sealed.data_key, &sealed.value, &class.name()],
					)
					.await?;
				class
			}
			Wanted::Class(class) => {
				let Some(kept) = kept else { return Ok(Set::Missing) };
				match (kept.named.class, class) {
					(held, wanted) if held == wanted => return Ok(Set::Unchanged),
					(Class::Secret, Class::Config) => return Ok(Set::Revealing),
					_ => {}
				}
				transaction
					.execute(
						"UPDATE secrets SET class = $4, updated_at = now()
						WHERE scope = $1 AND app = $2 AND name = $3",
						&[&scope, &app, &name, &class.name()],
					)
					.await?;
				class
			}
		};
		let id: i64 = transaction
			.query_one(
				"INSERT INTO changes (by, scope, app, name, class) VALUES ($1, $2, $3, $4, $5)
				RETURNING id",
				&[&by, &scope, &app, &name, &class.name()],
			)
			.await?
			.get(0);
		transaction.commit().await?;
		Ok(Set::Changed(id))
	}

	/// Records that `by` compared a value offered against `identity`'s, and what it answered, unless
	/// the secret has answered `COMPARISONS` in the last `WINDOW`, then answers how long until one
	/// is let through. Counted and written under one lock in one transaction, so the cores share one
	/// count and two of them never both pass the last; what is refused is not counted. The value
	/// offered is never kept.
	pub async fn compared(
		&self,
		identity: &Identity<'_>,
		by: &str,
		matches: bool,
	) -> anyhow::Result<Compared> {
		let Identity { scope, app, name } = *identity;
		let window = WINDOW.as_secs_f64();
		let mut client = self.pool.get().await?;
		let transaction = client.transaction().await?;
		transaction.execute("SELECT pg_advisory_xact_lock($1)", &[&COMPARES]).await?;
		let counted = transaction
			.query_one(
				"SELECT count(*), extract(epoch FROM
					min(at) + make_interval(secs => $4) - clock_timestamp())::float8
				FROM comparisons WHERE scope = $1 AND app = $2 AND name = $3
					AND at > clock_timestamp() - make_interval(secs => $4)",
				&[&scope, &app, &name, &window],
			)
			.await?;
		if counted.get::<_, i64>(0) >= COMPARISONS {
			let wait: Option<f64> = counted.get(1);
			return Ok(Compared::Limited(Duration::from_secs_f64(
				wait.unwrap_or(window).clamp(0.0, window),
			)));
		}
		let id: i64 = transaction
			.query_one(
				"INSERT INTO comparisons (at, by, scope, app, name, matches)
				VALUES (clock_timestamp(), $1, $2, $3, $4, $5) RETURNING id",
				&[&by, &scope, &app, &name, &matches],
			)
			.await?
			.get(0);
		transaction.commit().await?;
		Ok(Compared::Kept(id))
	}
}

fn kept(row: &tokio_postgres::Row) -> Kept {
	Kept {
		named: Named {
			scope: row.get(0),
			app: row.get(1),
			name: row.get(2),
			class: Class::from_name(row.get(3)),
			changed: row.get(4),
			revision: row.get(7),
		},
		sealed: Sealed { data_key: row.get(5), value: row.get(6) },
	}
}

#[cfg(test)]
pub(crate) mod testing;

#[cfg(test)]
mod tests;
