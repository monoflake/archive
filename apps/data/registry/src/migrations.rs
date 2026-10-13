//! The registry's schema, applied by the registry itself at start, one migration at a time, under
//! an advisory lock so two registries starting together apply each once. A migration only adds --
//! an expand -- unless it is a contract, which removes what no running version reads any more and
//! ships a release after the expand that made it unused. The pattern is the ledger's; see
//! spec/architecture/ledger.md, "Kept for good, in the platform's Postgres".

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

/// In order, never edited once released: a change is a new one. A change row is appended and never
/// updated or deleted; see spec/architecture/registry.md.
pub const MIGRATIONS: &[Migration] = &[
	Migration {
		version: 1,
		name: "nodes and their changes",
		kind: Kind::Expand,
		sql: r#"
		CREATE TABLE nodes (
			name text COLLATE "C" PRIMARY KEY,
			record jsonb NOT NULL,
			updated_at timestamptz NOT NULL DEFAULT now()
		);
		CREATE TABLE changes (
			id bigserial PRIMARY KEY,
			at timestamptz NOT NULL DEFAULT now(),
			by text NOT NULL,
			name text COLLATE "C" NOT NULL,
			before jsonb,
			after jsonb
		);
	"#,
	},
	// Every row written before it is a node's, which the default says; a resource's revision is its
	// newest change, read by kind and name. See RFC cpc2b.
	Migration {
		version: 2,
		name: "apps, and the kind of each change",
		kind: Kind::Expand,
		sql: r#"
			ALTER TABLE changes ADD COLUMN kind text COLLATE "C" NOT NULL DEFAULT 'node';
			CREATE INDEX changes_by_resource ON changes (kind, name, id);
			CREATE TABLE apps (
				name text COLLATE "C" PRIMARY KEY,
				spec jsonb NOT NULL,
				updated_at timestamptz NOT NULL DEFAULT now()
			);
		"#,
	},
	// `registry_id()` makes every id: 64 bits of a version 4 UUID, Postgres's strong random source,
	// the version and variant nibbles left out, as 16 hexadecimal digits. As a default it fills each
	// row held, and every row an older release inserts; store.rs draws its own, past a collision. A
	// history row takes the id of the resource holding its name now, unless a delete since parted
	// the two, and no task, as none was made before. See RFC 2kuvi and RFC cpc2b.
	Migration {
		version: 3,
		name: "an id for every resource, and the resource and task of each change",
		kind: Kind::Expand,
		sql: r#"
			CREATE FUNCTION registry_id() RETURNS text LANGUAGE sql VOLATILE AS $$
				SELECT substr(u, 1, 12) || substr(u, 14, 3) || substr(u, 18, 1)
				FROM (SELECT replace(gen_random_uuid()::text, '-', '') AS u) uuid
			$$;
			ALTER TABLE nodes ADD COLUMN id text COLLATE "C" NOT NULL DEFAULT registry_id();
			ALTER TABLE apps ADD COLUMN id text COLLATE "C" NOT NULL DEFAULT registry_id();
			CREATE UNIQUE INDEX nodes_by_id ON nodes (id);
			CREATE UNIQUE INDEX apps_by_id ON apps (id);
			ALTER TABLE changes ADD COLUMN resource text COLLATE "C";
			ALTER TABLE changes ADD COLUMN task text COLLATE "C";
			CREATE UNIQUE INDEX changes_by_task ON changes (task);
			UPDATE changes c SET resource = r.id
				FROM (
					SELECT 'node' AS kind, name, id FROM nodes UNION ALL SELECT 'app', name, id FROM apps
				) r
				WHERE c.kind = r.kind AND c.name = r.name AND c.id > coalesce(
					(SELECT max(d.id) FROM changes d
						WHERE d.kind = c.kind AND d.name = c.name AND d.after IS NULL),
					0
				);
		"#,
	},
	// Each kept as an app is, its id made as every id is. See RFC 32hd9.
	Migration {
		version: 4,
		name: "accounts and domains",
		kind: Kind::Expand,
		sql: r#"
			CREATE TABLE accounts (
				name text COLLATE "C" PRIMARY KEY,
				id text COLLATE "C" NOT NULL DEFAULT registry_id(),
				spec jsonb NOT NULL,
				updated_at timestamptz NOT NULL DEFAULT now()
			);
			CREATE UNIQUE INDEX accounts_by_id ON accounts (id);
			CREATE TABLE domains (
				name text COLLATE "C" PRIMARY KEY,
				id text COLLATE "C" NOT NULL DEFAULT registry_id(),
				spec jsonb NOT NULL,
				updated_at timestamptz NOT NULL DEFAULT now()
			);
			CREATE UNIQUE INDEX domains_by_id ON domains (id);
		"#,
	},
	// An account has no name and is kept by its id, a change naming none; the `domain` kind leaves,
	// and with it every node's `domain` and an `account` holding a provider's code, never an id. Each
	// is a change in the history as a write is, by `migration`; a resource's revision is now read by
	// its id, so every change to one held is given it first. See RFC 32hd9 and
	// spec/architecture/registry.md.
	Migration {
		version: 5,
		name: "accounts by id alone, and no domains",
		kind: Kind::Contract,
		sql: r#"
			ALTER TABLE accounts DROP CONSTRAINT accounts_pkey;
			ALTER TABLE accounts ADD CONSTRAINT accounts_pkey PRIMARY KEY USING INDEX accounts_by_id;
			ALTER TABLE accounts ALTER COLUMN name DROP NOT NULL;
			ALTER TABLE changes ALTER COLUMN name DROP NOT NULL;
			UPDATE changes c SET resource = r.id
				FROM (
					SELECT 'node' AS kind, name, id FROM nodes
					UNION ALL SELECT 'app', name, id FROM apps
					UNION ALL SELECT 'account', name, id FROM accounts
				) r
				WHERE c.resource IS NULL AND c.kind = r.kind AND c.name = r.name AND c.id > coalesce(
					(SELECT max(d.id) FROM changes d
						WHERE d.kind = c.kind AND d.name = c.name AND d.after IS NULL),
					0
				);
			CREATE INDEX changes_by_resource_id ON changes (kind, resource, id);

			INSERT INTO changes (kind, by, name, before, after, resource, task)
				SELECT 'domain', 'migration', name, spec, NULL, id, registry_id()
				FROM domains ORDER BY name;
			DROP TABLE domains;

			CREATE FUNCTION registry_stripped(record jsonb) RETURNS jsonb LANGUAGE sql IMMUTABLE AS $$
				SELECT CASE
					WHEN record->>'account' !~ '^[0-9a-f]{16}$' THEN record - 'domain' - 'account'
					ELSE record - 'domain'
				END
			$$;
			INSERT INTO changes (kind, by, name, before, after, resource, task)
				SELECT 'node', 'migration', name, record, registry_stripped(record), id, registry_id()
				FROM nodes WHERE record <> registry_stripped(record) ORDER BY name;
			UPDATE nodes SET record = registry_stripped(record), updated_at = now()
				WHERE record <> registry_stripped(record);
			DROP FUNCTION registry_stripped(jsonb);

			INSERT INTO changes (kind, by, name, before, after, resource, task)
				SELECT 'account', 'migration', name, spec, spec, id, registry_id()
				FROM accounts WHERE name IS NOT NULL ORDER BY name;
			UPDATE accounts SET name = NULL, updated_at = now() WHERE name IS NOT NULL;
		"#,
	},
];

/// The advisory lock every registry takes before migrating: "registry" in ASCII.
const LOCK: i64 = 0x7265_6769_7374_7279;

const RECORDED: &str = "CREATE TABLE IF NOT EXISTS registry_migrations (
	version integer PRIMARY KEY,
	name text NOT NULL,
	applied_at timestamptz NOT NULL DEFAULT now()
)";

/// Every migration not yet applied, each in its own transaction with its record, on a connection of
/// its own: the lock is the session's, so it goes when the connection does, whatever happens.
/// Answers the versions applied now.
pub async fn run(client: &mut Client) -> Result<Vec<i32>, tokio_postgres::Error> {
	apply(client, MIGRATIONS).await
}

/// `run` over `migrations` alone, so a test can stand a schema as an older release left it.
pub(crate) async fn apply(
	client: &mut Client,
	migrations: &[Migration],
) -> Result<Vec<i32>, tokio_postgres::Error> {
	client.execute("SELECT pg_advisory_lock($1)", &[&LOCK]).await?;
	client.batch_execute(RECORDED).await?;
	let applied: Vec<i32> = client
		.query("SELECT version FROM registry_migrations", &[])
		.await?
		.iter()
		.map(|row| row.get(0))
		.collect();
	let mut now = Vec::new();
	for migration in migrations.iter().filter(|m| !applied.contains(&m.version)) {
		let transaction = client.transaction().await?;
		transaction.batch_execute(migration.sql).await?;
		transaction
			.execute(
				"INSERT INTO registry_migrations (version, name) VALUES ($1, $2)",
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
	fn the_lock_spells_registry() {
		assert_eq!(LOCK.to_be_bytes(), *b"registry");
	}
}
