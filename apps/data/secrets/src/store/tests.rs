use super::testing::store;
use super::{Kept, Set};
use crate::envelope::{self, Identity, Sealed};
use crate::seal::RootKey;

const ID: Identity = Identity { scope: "platform", app: "gemini", name: "GEMINI_KEY" };

fn never(_: &Sealed) -> bool {
	false
}

#[tokio::test]
async fn starts_empty_at_change_zero() {
	let Some(store) = store().await else { return };
	let listed = store.list().await.unwrap();
	assert!(listed.items.is_empty());
	assert_eq!(listed.revision, 0);
	assert_eq!(store.any().await.unwrap(), None);
	assert!(store.migrate().await.unwrap().is_empty(), "applied twice");
}

#[tokio::test]
async fn a_write_is_held_to_the_revision_it_read() {
	let Some(store) = store().await else { return };
	let root = RootKey::generate();
	let first = envelope::seal(&root, &ID, b"one");
	assert_eq!(store.set(&ID, "test", 3, &first, never).await.unwrap(), Set::Stale);
	let Set::Changed(created) = store.set(&ID, "alice", 0, &first, never).await.unwrap() else {
		panic!("not written")
	};
	assert_eq!(store.set(&ID, "test", 0, &first, never).await.unwrap(), Set::Stale);
	let second = envelope::seal(&root, &ID, b"two");
	let Set::Changed(changed) = store.set(&ID, "bob", created, &second, never).await.unwrap() else {
		panic!("not written")
	};
	assert!(changed > created);
	assert_eq!(store.set(&ID, "test", created, &second, never).await.unwrap(), Set::Stale);

	let listed = store.list().await.unwrap();
	assert_eq!(listed.revision, changed);
	assert_eq!(listed.items.len(), 1);
	assert_eq!(listed.items[0].revision, changed);
	let (kept, revision) = store.of_apps(&["gemini".into()]).await.unwrap();
	assert_eq!(revision, changed);
	assert_eq!(
		envelope::open(&root, &kept[0].identity(), &kept[0].sealed).unwrap().as_slice(),
		b"two"
	);
}

#[tokio::test]
async fn a_write_of_the_value_kept_writes_no_change() {
	let Some(store) = store().await else { return };
	let root = RootKey::generate();
	let Set::Changed(created) =
		store.set(&ID, "test", 0, &envelope::seal(&root, &ID, b"one"), never).await.unwrap()
	else {
		panic!("not written")
	};
	let again = envelope::seal(&root, &ID, b"one");
	let same = |kept: &Sealed| envelope::open(&root, &ID, kept).unwrap().as_slice() == b"one";
	assert_eq!(store.set(&ID, "test", created, &again, same).await.unwrap(), Set::Unchanged);
	assert_eq!(store.list().await.unwrap().revision, created);
}

#[tokio::test]
async fn a_change_row_says_who_and_which_and_never_what() {
	let Some(store) = store().await else { return };
	let root = RootKey::generate();
	store.set(&ID, "alice", 0, &envelope::seal(&root, &ID, b"s3cret"), never).await.unwrap();
	let client = store.pool.get().await.unwrap();
	let row = client.query_one("SELECT row_to_json(c)::text FROM changes c", &[]).await.unwrap();
	let text: String = row.get(0);
	assert!(text.contains("\"by\":\"alice\"") && text.contains("GEMINI_KEY"), "{text}");
	assert!(!text.contains("s3cret"));
	let columns: Vec<String> = client
		.query(
			"SELECT column_name::text FROM information_schema.columns
			WHERE table_name = 'changes' AND table_schema = current_schema() ORDER BY 1",
			&[],
		)
		.await
		.unwrap()
		.iter()
		.map(|row| row.get(0))
		.collect();
	assert_eq!(columns, ["app", "at", "by", "id", "name", "scope"]);
	let stored: Vec<u8> = client.query_one("SELECT value FROM secrets", &[]).await.unwrap().get(0);
	assert!(!stored.windows(6).any(|window| window == b"s3cret"));
}

#[tokio::test]
async fn secrets_are_answered_by_app_and_in_order() {
	let Some(store) = store().await else { return };
	let root = RootKey::generate();
	for (scope, app, name) in [
		("platform", "grok", "B"),
		("platform", "gemini", "B"),
		("canmi", "gemini", "A"),
		("platform", "gemini", "A"),
		("platform", "shot", "A"),
	] {
		let identity = Identity { scope, app, name };
		store
			.set(&identity, "test", 0, &envelope::seal(&root, &identity, name.as_bytes()), never)
			.await
			.unwrap();
	}
	let listed = store.list().await.unwrap();
	let order: Vec<(&str, &str, &str)> = listed
		.items
		.iter()
		.map(|item| (item.scope.as_str(), item.app.as_str(), item.name.as_str()))
		.collect();
	assert_eq!(order[0], ("canmi", "gemini", "A"));
	assert_eq!(order.len(), 5);

	let (kept, revision) = store.of_apps(&["gemini".into(), "shot".into()]).await.unwrap();
	assert_eq!(revision, listed.revision);
	let names: Vec<String> = kept
		.iter()
		.map(|Kept { named, .. }| format!("{}/{}/{}", named.scope, named.app, named.name))
		.collect();
	assert_eq!(
		names,
		["canmi/gemini/A", "platform/gemini/A", "platform/gemini/B", "platform/shot/A"]
	);
	for kept in &kept {
		assert_eq!(
			envelope::open(&root, &kept.identity(), &kept.sealed).unwrap().as_slice(),
			kept.named.name.as_bytes()
		);
	}
	assert!(store.of_apps(&[]).await.unwrap().0.is_empty());
	assert!(store.any().await.unwrap().is_some());
}
