//! The SQL is run against a Postgres only where `LEDGER_TEST_DATABASE_URL` names one, each test
//! in a schema of its own: `mise run test-sql` starts one, and CI has its own. Without it those
//! tests pass over themselves and say so, except in CI, where that is a failure.

use super::Store;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_postgres::config::Host;

static NEXT: AtomicU32 = AtomicU32::new(0);

/// A schema of its own in the test database, or None, said so, when there is no database.
async fn schema() -> Option<(String, String)> {
	let Ok(url) = std::env::var("LEDGER_TEST_DATABASE_URL") else {
		let ci = std::env::var("CI").is_ok_and(|value| !value.is_empty());
		assert!(!ci, "CI is set and LEDGER_TEST_DATABASE_URL is not: CI runs every SQL test");
		eprintln!("LEDGER_TEST_DATABASE_URL is not set; this test's SQL is not run");
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
	Some((url, schema))
}

pub async fn store() -> Option<Store> {
	let (url, schema) = schema().await?;
	let store = Store::connect(&url, Some(&schema)).unwrap();
	store.migrate().await.unwrap();
	Some(store)
}

/// A store whose every byte to and from Postgres is held `delay` each way, as a primary an
/// ocean away holds it: what costs a round trip per row shows here.
pub async fn store_behind(delay: Duration) -> Option<Store> {
	let (url, schema) = schema().await?;
	Store::connect(&url, Some(&schema)).unwrap().migrate().await.unwrap();
	let config: tokio_postgres::Config = url.parse().unwrap();
	let Host::Tcp(host) = &config.get_hosts()[0] else { panic!("a TCP host, please") };
	let port = proxy((host.clone(), config.get_ports()[0]), delay).await;
	let mut near = tokio_postgres::Config::new();
	near.host("127.0.0.1").port(port).user(config.get_user().unwrap());
	near.dbname(config.get_dbname().unwrap());
	if let Some(password) = config.get_password() {
		near.password(password);
	}
	Some(Store::with(near, Some(&schema)).unwrap())
}

async fn proxy(upstream: (String, u16), delay: Duration) -> u16 {
	let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
	let port = listener.local_addr().unwrap().port();
	tokio::spawn(async move {
		while let Ok((client, _)) = listener.accept().await {
			let upstream = upstream.clone();
			tokio::spawn(async move {
				let Ok(server) = TcpStream::connect((upstream.0.as_str(), upstream.1)).await else {
					return;
				};
				let ((client_in, client_out), (server_in, server_out)) =
					(client.into_split(), server.into_split());
				tokio::join!(late(client_in, server_out, delay), late(server_in, client_out, delay));
			});
		}
	});
	port
}

/// `from` copied to `to`, each piece `delay` after it arrived and none held back by another.
async fn late(mut from: OwnedReadHalf, mut to: OwnedWriteHalf, delay: Duration) {
	let (send, mut received) = mpsc::unbounded_channel::<(Instant, Vec<u8>)>();
	let reading = async move {
		let mut buffer = vec![0; 64 * 1024];
		while let Ok(read) = from.read(&mut buffer).await {
			if read == 0 || send.send((Instant::now() + delay, buffer[..read].to_vec())).is_err() {
				break;
			}
		}
	};
	let writing = async move {
		while let Some((due, bytes)) = received.recv().await {
			tokio::time::sleep_until(due).await;
			if to.write_all(&bytes).await.is_err() {
				break;
			}
		}
		let _ = to.shutdown().await;
	};
	tokio::join!(reading, writing);
}

/// A store over a database that is never reached, for what is refused before the store is asked.
pub fn unreached() -> Store {
	Store::connect("host=unreached.invalid user=nobody dbname=none", None).unwrap()
}
