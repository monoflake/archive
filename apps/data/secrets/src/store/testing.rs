//! The SQL is run against a Postgres only where `SECRETS_TEST_DATABASE_URL` names one, each test
//! in a schema of its own: `mise run test-sql` starts one, and CI has its own. Without it those
//! tests pass over themselves and say so, except in CI, where that is a failure.

use super::Store;
use std::sync::atomic::{AtomicU32, Ordering};

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A store in a schema of its own in the test database, or None, said so, when there is none.
pub async fn store() -> Option<Store> {
	let store = bare().await?;
	store.migrate().await.unwrap();
	Some(store)
}

/// `store`, its schema left empty and no migration applied.
pub async fn bare() -> Option<Store> {
	let Ok(url) = std::env::var("SECRETS_TEST_DATABASE_URL") else {
		let ci = std::env::var("CI").is_ok_and(|value| !value.is_empty());
		assert!(!ci, "CI is set and SECRETS_TEST_DATABASE_URL is not: CI runs every SQL test");
		eprintln!("SECRETS_TEST_DATABASE_URL is not set; this test's SQL is not run");
		return None;
	};
	let schema = format!(
		"test_{}_{}_{}",
		std::process::id(),
		jiff::Timestamp::now().as_millisecond(),
		NEXT.fetch_add(1, Ordering::Relaxed)
	);
	let (client, connection) = tokio_postgres::connect(&url, tokio_postgres::NoTls).await.unwrap();
	tokio::spawn(connection);
	client.batch_execute(&format!("CREATE SCHEMA {schema}")).await.unwrap();
	Some(Store::connect(&url, Some(&schema)).unwrap())
}

/// A store over a database that is never reached, for what is refused before the store is asked.
pub fn unreached() -> Store {
	Store::connect("host=unreached.invalid user=nobody dbname=none", None).unwrap()
}

/// Every comparison `store` has kept, oldest first: who, which name, and the answer.
pub async fn comparisons(store: &Store) -> Vec<(String, String, bool)> {
	store
		.pool
		.get()
		.await
		.unwrap()
		.query("SELECT by, name, matches FROM comparisons ORDER BY id", &[])
		.await
		.unwrap()
		.iter()
		.map(|row| (row.get(0), row.get(1), row.get(2)))
		.collect()
}
