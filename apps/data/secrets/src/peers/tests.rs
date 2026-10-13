use super::*;
use crate::api::{
	self,
	tests::{PEER, nobody, secrets, serve},
};
use crate::seal::RootKey;
use crate::store::testing::unreached;

/// A core's secret service, sealed and holding `share`, served on loopback.
async fn core_holding(share: Option<Share>) -> String {
	let seal = Arc::new(Seal::default());
	if let Some(share) = share {
		seal.hold(share);
	}
	serve(api::routes(secrets(unreached(), seal, Vec::new()))).await
}

fn scratch(name: &str) -> std::path::PathBuf {
	let directory = std::env::temp_dir().join(format!("secrets-peers-{}-{name}", std::process::id()));
	std::fs::create_dir_all(&directory).unwrap();
	directory
}

#[test]
fn the_cores_are_read_as_host_reads_them() {
	let cores = Cores::parse(" 100.64.0.1\n100.64.0.2 ").unwrap();
	let at = |port: u16| ["100.64.0.1", "100.64.0.2"].map(|host| format!("http://{host}:{port}"));
	assert_eq!(cores.peers, at(12014));
	assert_eq!(cores.registries, at(12013));
	assert_eq!(Cores::parse("100.64.0.1 tyo"), Err("tyo".into()));
	assert_eq!(Cores::parse("").unwrap(), Cores::default());
}

#[test]
fn the_port_is_the_one_the_declaration_states() {
	let declaration = include_str!("../../service.toml");
	assert!(declaration.lines().any(|line| line.trim() == format!("port = {PORT}")));
}

#[tokio::test]
async fn two_sealed_cores_open_a_third_and_one_alone_does_not() {
	let key = RootKey::generate();
	let shares = seal::split(&key);
	let peers = vec![
		core_holding(Some(shares[1].clone())).await,
		core_holding(None).await,
		nobody(),
	];
	let directory = scratch("open");
	let path = directory.join("share");
	std::fs::write(&path, shares[0].to_json().as_bytes()).unwrap();

	let seal = Arc::new(Seal::default());
	let every = Duration::from_millis(20);
	tokio::time::timeout(
		Duration::from_secs(10),
		unseal(seal.clone(), client(), peers, PEER.into(), path, every),
	)
	.await
	.expect("two shares open the lock");
	assert_eq!(seal.with_key(|held| *held.bytes()), Some(*key.bytes()));
	assert!(seal.share().unwrap() == shares[0]);

	let alone = Arc::new(Seal::default());
	let lone = vec![core_holding(None).await];
	let path = directory.join("share");
	let waited = tokio::time::timeout(
		Duration::from_millis(300),
		unseal(alone.clone(), client(), lone, PEER.into(), path, every),
	)
	.await;
	assert!(waited.is_err(), "one share opened the lock");
	assert!(alone.is_sealed());
}

#[tokio::test]
async fn a_core_with_no_file_is_opened_by_two_peers_and_a_wrong_token_by_none() {
	let key = RootKey::generate();
	let shares = seal::split(&key);
	let peers =
		vec![core_holding(Some(shares[1].clone())).await, core_holding(Some(shares[2].clone())).await];
	let missing = scratch("missing").join("share");
	let every = Duration::from_millis(20);

	let refused = Arc::new(Seal::default());
	let waited = tokio::time::timeout(
		Duration::from_millis(300),
		unseal(refused.clone(), client(), peers.clone(), "wrong".into(), missing.clone(), every),
	)
	.await;
	assert!(waited.is_err() && refused.is_sealed());

	let seal = Arc::new(Seal::default());
	tokio::time::timeout(
		Duration::from_secs(10),
		unseal(seal.clone(), client(), peers, PEER.into(), missing, every),
	)
	.await
	.expect("two peers open the lock");
	assert_eq!(seal.with_key(|held| *held.bytes()), Some(*key.bytes()));
	assert!(seal.share().is_none());
}

#[tokio::test]
async fn a_share_file_that_is_not_one_is_not_held() {
	let path = scratch("bad").join("share");
	std::fs::write(&path, "{}").unwrap();
	assert!(read_share(&path).is_err());
	assert!(read_share(&path.with_file_name("absent")).is_err());
}
