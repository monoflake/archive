//! The secret service's schema, applied by the service itself at start, one migration at a time,
//! under an advisory lock so two copies starting together apply each once. The pattern is the
//! registry's and the ledger's; see spec/architecture/ledger.md, "Kept for good, in the platform's
//! Postgres".

use tokio_postgres::Client;

/// Whether a migration only adds, or removes what an earlier one made unused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
	Expand,
	Contract,
}

pub struct Migration {
	pub version: i32,
	pub name: &'static str,
	pub kind: Kind,
	pub sql: &'static str,
}

/// In order, never edited once released: a change is a new one. A secret's row holds ciphertext
/// alone, and a change row says who wrote which secret and when, never what; it is appended and
/// never updated or deleted. See RFC 231yq.
pub const MIGRATIONS: &[Migration] = &[Migration {
	version: 1,
	name: "secrets and their changes",
	kind: Kind::Expand,
	sql: r#"
		CREATE TABLE secrets (
			scope text COLLATE "C" NOT NULL,
			app text COLLATE "C" NOT NULL,
			name text COLLATE "C" NOT NULL,
			data_key bytea NOT NULL,
			value bytea NOT NULL,
			updated_at timestamptz NOT NULL DEFAULT now(),
			PRIMARY KEY (scope, app, name)
		);
		CREATE TABLE changes (
			id bigserial PRIMARY KEY,
			at timestamptz NOT NULL DEFAULT now(),
			by text NOT NULL,
			scope text COLLATE "C" NOT NULL,
			app text COLLATE "C" NOT NULL,
			name text COLLATE "C" NOT NULL
		);
		CREATE INDEX changes_by_secret ON changes (scope, app, name, id);
	"#,
}];

/// The advisory lock every copy takes before migrating: "secrets" in ASCII.
const LOCK: i64 = 0x73_6563_7265_7473;

const RECORDED: &str = "CREATE TABLE IF NOT EXISTS secrets_migrations (
	version integer PRIMARY KEY,
	name text NOT NULL,
	applied_at timestamptz NOT NULL DEFAULT now()
)";

/// Every migration not yet applied, each in its own transaction with its record, on a connection of
/// its own: the lock is the session's, so it goes when the connection does, whatever happens.
/// Answers the versions applied now.
pub async fn run(client: &mut Client) -> Result<Vec<i32>, tokio_postgres::Error> {
	client.execute("SELECT pg_advisory_lock($1)", &[&LOCK]).await?;
	client.batch_execute(RECORDED).await?;
	let applied: Vec<i32> = client
		.query("SELECT version FROM secrets_migrations", &[])
		.await?
		.iter()
		.map(|row| row.get(0))
		.collect();
	let mut now = Vec::new();
	for migration in MIGRATIONS.iter().filter(|m| !applied.contains(&m.version)) {
		let transaction = client.transaction().await?;
		transaction.batch_execute(migration.sql).await?;
		transaction
			.execute(
				"INSERT INTO secrets_migrations (version, name) VALUES ($1, $2)",
				&[&migration.version, &migration.name],
			)
			.await?;
		transaction.commit().await?;
		now.push(migration.version);
	}
	client.execute("SELECT pg_advisory_unlock($1)", &[&LOCK]).await?;
	Ok(now)
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn versions_climb_by_one_from_one() {
		for (index, migration) in MIGRATIONS.iter().enumerate() {
			assert_eq!(migration.version, i32::try_from(index).unwrap() + 1, "{}", migration.name);
		}
	}

	#[test]
	fn an_expand_only_adds() {
		for migration in MIGRATIONS.iter().filter(|m| m.kind == Kind::Expand) {
			let sql = migration.sql.to_uppercase();
			for removing in ["DROP ", "RENAME ", " TYPE ", "SET NOT NULL", "TRUNCATE "] {
				assert!(!sql.contains(removing), "{} holds {removing:?}", migration.name);
			}
		}
	}

	#[test]
	fn every_table_has_a_primary_key() {
		for migration in MIGRATIONS {
			for table in migration.sql.split("CREATE TABLE").skip(1) {
				let body = table.split(");").next().unwrap();
				assert!(body.contains("PRIMARY KEY"), "{}: {body}", migration.name);
			}
		}
	}

	#[test]
	fn no_change_row_can_hold_a_value() {
		let changes = MIGRATIONS[0].sql.split("CREATE TABLE changes").nth(1).unwrap();
		let changes = changes.split(");").next().unwrap();
		assert!(!changes.contains("value") && !changes.contains("bytea"), "{changes}");
	}

	#[test]
	fn the_lock_spells_secrets() {
		assert_eq!(&LOCK.to_be_bytes()[1..], b"secrets");
	}
}
