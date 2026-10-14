//! The ledger's schema, applied by the ledger itself at start, one migration at a time, under an
//! advisory lock so two ledgers starting together apply each once. A migration only adds -- an
//! expand -- unless it is a contract, which removes what no running version reads any more and
//! ships a release after the expand that made it unused. See spec/architecture/ledger.md, "Kept
//! for good".

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

/// In order, never edited once released: a change is a new one. Every table has a primary key;
/// these are the task's and the event's own, so none is minted.
pub const MIGRATIONS: &[Migration] = &[
	Migration {
		version: 1,
		name: "tasks and their events",
		kind: Kind::Expand,
		sql: r#"
		CREATE TABLE tasks (
			service text COLLATE "C" NOT NULL,
			id text COLLATE "C" NOT NULL,
			kind text NOT NULL,
			state text NOT NULL,
			caller text NOT NULL,
			updated_at bigint NOT NULL,
			finished_at bigint,
			asked_at bigint NOT NULL,
			parent_service text COLLATE "C",
			parent_id text COLLATE "C",
			record jsonb NOT NULL,
			PRIMARY KEY (service, id)
		);
		CREATE INDEX tasks_updated_at ON tasks (updated_at, id);
		CREATE INDEX tasks_parent ON tasks (parent_service, parent_id, updated_at, id);
		CREATE INDEX tasks_asked_at ON tasks (asked_at, service, state);
		CREATE TABLE events (
			service text COLLATE "C" NOT NULL,
			task text COLLATE "C" NOT NULL,
			seq bigint NOT NULL,
			at bigint NOT NULL,
			stage text NOT NULL,
			level text NOT NULL,
			message text NOT NULL,
			data jsonb NOT NULL,
			PRIMARY KEY (service, task, seq)
		);
	"#,
	},
	Migration {
		version: 2,
		name: "runs",
		kind: Kind::Expand,
		sql: r#"
		CREATE TABLE runs (
			service text COLLATE "C" NOT NULL,
			id text COLLATE "C" NOT NULL,
			workflow text NOT NULL,
			class text NOT NULL CHECK (class IN ('quorum', 'local')),
			status text NOT NULL CHECK (status IN
				('ready', 'running', 'sleeping', 'waiting', 'done', 'failed')),
			params jsonb NOT NULL,
			output jsonb,
			error text,
			run_at bigint NOT NULL,
			fence bigint NOT NULL DEFAULT 0,
			created_at bigint NOT NULL,
			updated_at bigint NOT NULL,
			finished_at bigint,
			PRIMARY KEY (service, id),
			FOREIGN KEY (service, id) REFERENCES tasks (service, id)
		);
		CREATE INDEX runs_due ON runs (run_at, service, id)
			WHERE status IN ('ready', 'sleeping', 'waiting');
	"#,
	},
	Migration {
		version: 3,
		name: "steps and the events sent to a run",
		kind: Kind::Expand,
		sql: r#"
		CREATE TABLE steps (
			service text COLLATE "C" NOT NULL,
			run text COLLATE "C" NOT NULL,
			name text COLLATE "C" NOT NULL,
			kind text NOT NULL CHECK (kind IN ('do', 'sleep', 'wait')),
			status text NOT NULL CHECK (status IN
				('started', 'retrying', 'done', 'failed', 'sleeping', 'waiting')),
			attempt integer NOT NULL DEFAULT 0,
			starts integer NOT NULL DEFAULT 0,
			config jsonb NOT NULL,
			result jsonb CHECK (octet_length(result::text) <= 2097152),
			error text,
			deadline bigint,
			wake_at bigint,
			holder text COLLATE "C",
			fence bigint,
			placement jsonb,
			started_at bigint NOT NULL,
			finished_at bigint,
			PRIMARY KEY (service, run, name),
			FOREIGN KEY (service, run) REFERENCES runs (service, id)
		);
		CREATE TABLE run_events (
			service text COLLATE "C" NOT NULL,
			run text COLLATE "C" NOT NULL,
			key text COLLATE "C" NOT NULL,
			event_type text NOT NULL,
			payload jsonb NOT NULL,
			at bigint NOT NULL,
			taken_by text COLLATE "C",
			PRIMARY KEY (service, run, key),
			FOREIGN KEY (service, run) REFERENCES runs (service, id)
		);
	"#,
	},
	Migration {
		version: 4,
		name: "leases and executors",
		kind: Kind::Expand,
		sql: r#"
		CREATE TABLE leases (
			service text COLLATE "C" NOT NULL,
			run text COLLATE "C" NOT NULL,
			holder text COLLATE "C" NOT NULL,
			fence bigint NOT NULL,
			expires_at bigint NOT NULL,
			granted_at bigint NOT NULL,
			PRIMARY KEY (service, run),
			FOREIGN KEY (service, run) REFERENCES runs (service, id)
		);
		CREATE INDEX leases_holder ON leases (holder);
		CREATE TABLE executors (
			node text COLLATE "C" NOT NULL,
			seen_at bigint NOT NULL,
			capacity integer NOT NULL,
			push_url text,
			PRIMARY KEY (node)
		);
	"#,
	},
	Migration {
		version: 5,
		name: "the workflows an executor runs",
		kind: Kind::Expand,
		sql: r#"
		ALTER TABLE executors ADD COLUMN workflows text[] NOT NULL DEFAULT '{}';
	"#,
	},
	Migration {
		version: 6,
		name: "the ends of a run's last turns",
		kind: Kind::Expand,
		sql: r#"
		ALTER TABLE runs ADD COLUMN turns text[] NOT NULL DEFAULT '{}';
	"#,
	},
];

/// The advisory lock every ledger takes before migrating: "ledger" in ASCII.
const LOCK: i64 = 0x6c65_6467_6572;

const RECORDED: &str = "CREATE TABLE IF NOT EXISTS ledger_migrations (
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
		.query("SELECT version FROM ledger_migrations", &[])
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
				"INSERT INTO ledger_migrations (version, name) VALUES ($1, $2)",
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
}
