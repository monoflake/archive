use super::testing::{bare, store};
use super::{COMPARISONS, Class, Compared, Kept, Set, Store, WINDOW, Wanted};
use crate::envelope::{self, Identity, Sealed};
use crate::migrations;
use crate::seal::RootKey;

const ID: Identity = Identity { scope: "platform", app: "gemini", name: "GEMINI_KEY" };

fn never(_: &Sealed) -> bool {
	false
}

fn secret(sealed: &Sealed) -> Wanted<'_> {
	Wanted::Value(sealed, Class::Secret)
}

/// Writes `value` as `class` where nothing is yet, answering the change's id.
async fn created(store: &Store, root: &RootKey, value: &[u8], class: Class) -> i64 {
	let sealed = envelope::seal(root, &ID, value);
	let Set::Changed(id) =
		store.set(&ID, "test", 0, Wanted::Value(&sealed, class), never).await.unwrap()
	else {
		panic!("not written")
	};
	id
}

/// What each column of `table` is called, in order of name.
async fn columns(store: &Store, table: &str) -> Vec<String> {
	store
		.pool
		.get()
		.await
		.unwrap()
		.query(
			"SELECT column_name::text FROM information_schema.columns
			WHERE table_name = $1 AND table_schema = current_schema() ORDER BY 1",
			&[&table],
		)
		.await
		.unwrap()
		.iter()
		.map(|row| row.get(0))
		.collect()
}

#[tokio::test]
async fn starts_empty_at_change_zero() {
	let Some(store) = store().await else { return };
	let (items, revision) = store.list(None).await.unwrap();
	assert!(items.is_empty());
	assert_eq!(revision, 0);
	assert_eq!(store.any().await.unwrap(), None);
	assert_eq!(store.get(&ID).await.unwrap(), None);
	assert!(store.migrate().await.unwrap().is_empty(), "applied twice");
}

#[tokio::test]
async fn a_write_is_held_to_the_revision_it_read() {
	let Some(store) = store().await else { return };
	let root = RootKey::generate();
	let first = envelope::seal(&root, &ID, b"one");
	assert_eq!(store.set(&ID, "test", 3, secret(&first), never).await.unwrap(), Set::Stale);
	let Set::Changed(created) = store.set(&ID, "alice", 0, secret(&first), never).await.unwrap()
	else {
		panic!("not written")
	};
	assert_eq!(store.set(&ID, "test", 0, secret(&first), never).await.unwrap(), Set::Stale);
	let second = envelope::seal(&root, &ID, b"two");
	let Set::Changed(changed) = store.set(&ID, "bob", created, secret(&second), never).await.unwrap()
	else {
		panic!("not written")
	};
	assert!(changed > created);
	assert_eq!(store.set(&ID, "test", created, secret(&second), never).await.unwrap(), Set::Stale);

	let (items, revision) = store.list(None).await.unwrap();
	assert_eq!(revision, changed);
	assert_eq!(items.len(), 1);
	assert_eq!(items[0].named.revision, changed);
	assert_eq!(items[0].named.class, Class::Secret);
	let (kept, revision) = store.of_apps(&["gemini".into()], &["platform".into()]).await.unwrap();
	assert_eq!(revision, changed);
	assert_eq!(
		envelope::open(&root, &kept[0].identity(), &kept[0].sealed).unwrap().as_slice(),
		b"two"
	);
	assert_eq!(store.get(&ID).await.unwrap().unwrap(), kept[0]);
}

#[tokio::test]
async fn a_write_of_the_value_and_class_kept_writes_no_change() {
	let Some(store) = store().await else { return };
	let root = RootKey::generate();
	let created = created(&store, &root, b"one", Class::Secret).await;
	let again = envelope::seal(&root, &ID, b"one");
	let same = |kept: &Sealed| envelope::open(&root, &ID, kept).unwrap().as_slice() == b"one";
	assert_eq!(store.set(&ID, "test", created, secret(&again), same).await.unwrap(), Set::Unchanged);
	assert_eq!(store.list(None).await.unwrap().1, created);
	// The same value in another class is a change.
	let config = Wanted::Value(&again, Class::Config);
	assert!(matches!(store.set(&ID, "test", created, config, same).await.unwrap(), Set::Changed(_)));
	assert_eq!(store.get(&ID).await.unwrap().unwrap().named.class, Class::Config);
}

#[tokio::test]
async fn a_class_alone_hides_a_config_and_never_shows_a_secret() {
	let Some(store) = store().await else { return };
	let root = RootKey::generate();
	let missing = store.set(&ID, "test", 0, Wanted::Class(Class::Secret), never).await.unwrap();
	assert_eq!(missing, Set::Missing);

	let created = created(&store, &root, b"shown", Class::Config).await;
	let kept = store.get(&ID).await.unwrap().unwrap();
	assert_eq!(kept.named.class, Class::Config);
	let config = Wanted::Class(Class::Config);
	assert_eq!(store.set(&ID, "test", created, config, never).await.unwrap(), Set::Unchanged);
	let Set::Changed(hidden) =
		store.set(&ID, "carol", created, Wanted::Class(Class::Secret), never).await.unwrap()
	else {
		panic!("not hidden")
	};
	let after = store.get(&ID).await.unwrap().unwrap();
	assert_eq!((after.named.class, after.named.revision), (Class::Secret, hidden));
	assert_eq!(after.sealed, kept.sealed, "a class alone changed the value");
	assert!(after.named.changed >= kept.named.changed);

	let shown = store.set(&ID, "test", hidden, Wanted::Class(Class::Config), never).await.unwrap();
	assert_eq!(shown, Set::Revealing);
	assert_eq!(store.get(&ID).await.unwrap().unwrap().named.class, Class::Secret);

	let rewritten = envelope::seal(&root, &ID, b"shown");
	let config = Wanted::Value(&rewritten, Class::Config);
	assert!(matches!(store.set(&ID, "dave", hidden, config, never).await.unwrap(), Set::Changed(_)));
	assert_eq!(store.get(&ID).await.unwrap().unwrap().named.class, Class::Config);

	let client = store.pool.get().await.unwrap();
	let classes: Vec<(String, Option<String>)> = client
		.query("SELECT by, class FROM changes ORDER BY id", &[])
		.await
		.unwrap()
		.iter()
		.map(|row| (row.get(0), row.get(1)))
		.collect();
	let class = |name: &str| Some(name.to_owned());
	assert_eq!(
		classes,
		[
			("test".into(), class("config")),
			("carol".into(), class("secret")),
			("dave".into(), class("config"))
		]
	);
}

#[tokio::test]
async fn a_value_written_before_classes_is_a_secret() {
	let Some(store) = bare().await else { return };
	let (mut client, connection) = store.config.connect(tokio_postgres::NoTls).await.unwrap();
	let driven = tokio::spawn(connection);
	migrations::apply(&mut client, &migrations::MIGRATIONS[..1]).await.unwrap();
	client
		.execute(
			"INSERT INTO secrets (scope, app, name, data_key, value) VALUES ('platform', 'gemini', $1, '', '')",
			&[&"GEMINI_KEY"],
		)
		.await
		.unwrap();
	drop(client);
	let _ = driven.await;
	assert_eq!(store.migrate().await.unwrap(), [2]);
	assert_eq!(store.get(&ID).await.unwrap().unwrap().named.class, Class::Secret);
}

#[tokio::test]
async fn a_change_row_says_who_and_which_and_never_what() {
	let Some(store) = store().await else { return };
	let root = RootKey::generate();
	let sealed = envelope::seal(&root, &ID, b"s3cret");
	store.set(&ID, "alice", 0, secret(&sealed), never).await.unwrap();
	let client = store.pool.get().await.unwrap();
	let row = client.query_one("SELECT row_to_json(c)::text FROM changes c", &[]).await.unwrap();
	let text: String = row.get(0);
	assert!(text.contains("\"by\":\"alice\"") && text.contains("GEMINI_KEY"), "{text}");
	assert!(!text.contains("s3cret"));
	assert_eq!(columns(&store, "changes").await, ["app", "at", "by", "class", "id", "name", "scope"]);
	let stored: Vec<u8> = client.query_one("SELECT value FROM secrets", &[]).await.unwrap().get(0);
	assert!(!stored.windows(6).any(|window| window == b"s3cret"));
}

#[tokio::test]
async fn a_comparison_is_a_row_of_its_own_and_moves_no_revision() {
	let Some(store) = store().await else { return };
	let root = RootKey::generate();
	let created = created(&store, &root, b"s3cret", Class::Secret).await;
	let Compared::Kept(first) = store.compared(&ID, "erin", false).await.unwrap() else { panic!() };
	let Compared::Kept(second) = store.compared(&ID, "erin", true).await.unwrap() else { panic!() };
	assert!(second > first);
	assert_eq!(store.get(&ID).await.unwrap().unwrap().named.revision, created);
	assert_eq!(store.list(None).await.unwrap().1, created);
	let client = store.pool.get().await.unwrap();
	let rows: Vec<(String, String, bool)> = client
		.query("SELECT by, name, matches FROM comparisons ORDER BY id", &[])
		.await
		.unwrap()
		.iter()
		.map(|row| (row.get(0), row.get(1), row.get(2)))
		.collect();
	assert_eq!(
		rows,
		[("erin".into(), "GEMINI_KEY".into(), false), ("erin".into(), "GEMINI_KEY".into(), true)]
	);
	assert_eq!(
		columns(&store, "comparisons").await,
		["app", "at", "by", "id", "matches", "name", "scope"]
	);
}

#[tokio::test]
async fn a_secret_answers_a_few_comparisons_a_window_counted_by_every_copy() {
	let Some(store) = store().await else { return };
	let other = Identity { name: "OTHER", ..ID };
	for _ in 0..COMPARISONS {
		assert!(matches!(store.compared(&ID, "test", false).await.unwrap(), Compared::Kept(_)));
	}
	let Compared::Limited(wait) = store.compared(&ID, "test", true).await.unwrap() else {
		panic!("a sixth was let through")
	};
	assert!(wait > WINDOW - std::time::Duration::from_secs(10) && wait <= WINDOW, "{wait:?}");
	assert!(matches!(store.compared(&other, "test", false).await.unwrap(), Compared::Kept(_)));
	let client = store.pool.get().await.unwrap();
	let kept: i64 = client
		.query_one("SELECT count(*) FROM comparisons WHERE name = 'GEMINI_KEY'", &[])
		.await
		.unwrap()
		.get(0);
	assert_eq!(kept, COMPARISONS, "a refusal was counted");

	// Rows older than the window let the next one through.
	client
		.execute(
			"UPDATE comparisons SET at = at - interval '61 seconds' WHERE name = 'GEMINI_KEY'",
			&[],
		)
		.await
		.unwrap();
	assert!(matches!(store.compared(&ID, "test", true).await.unwrap(), Compared::Kept(_)));
}

#[tokio::test]
async fn two_copies_asking_at_once_never_both_pass_the_last() {
	let Some(store) = store().await else { return };
	let store = std::sync::Arc::new(store);
	for _ in 0..COMPARISONS - 1 {
		store.compared(&ID, "test", false).await.unwrap();
	}
	let asking: Vec<_> = (0..4)
		.map(|_| {
			let store = store.clone();
			tokio::spawn(async move { store.compared(&ID, "test", false).await.unwrap() })
		})
		.collect();
	let mut answered = Vec::new();
	for asked in asking {
		answered.push(asked.await.unwrap());
	}
	let kept = answered.iter().filter(|answer| matches!(answer, Compared::Kept(_))).count();
	assert_eq!(kept, 1, "{answered:?}");
}

#[tokio::test]
async fn secrets_are_answered_by_app_by_scope_and_in_order() {
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
		let sealed = envelope::seal(&root, &identity, name.as_bytes());
		store.set(&identity, "test", 0, secret(&sealed), never).await.unwrap();
	}
	let (items, revision) = store.list(None).await.unwrap();
	let order: Vec<(&str, &str, &str)> = items
		.iter()
		.map(|item| (item.named.scope.as_str(), item.named.app.as_str(), item.named.name.as_str()))
		.collect();
	assert_eq!(order[0], ("canmi", "gemini", "A"));
	assert_eq!(order.len(), 5);

	let (canmi, canmi_revision) = store.list(Some(("canmi", "gemini"))).await.unwrap();
	assert_eq!(canmi.len(), 1);
	assert_eq!((canmi[0].named.scope.as_str(), canmi[0].named.app.as_str()), ("canmi", "gemini"));
	assert_eq!(canmi_revision, canmi[0].named.revision);
	assert!(canmi_revision < revision);
	let (gemini, _) = store.list(Some(("platform", "gemini"))).await.unwrap();
	let names: Vec<&str> = gemini.iter().map(|kept| kept.named.name.as_str()).collect();
	assert_eq!(names, ["A", "B"], "another app of the scope was listed");
	assert_eq!(store.list(Some(("accounts", "gemini"))).await.unwrap(), (Vec::new(), 0));

	let every = ["platform".to_owned(), "canmi".to_owned()];
	let (kept, of_apps) = store.of_apps(&["gemini".into(), "shot".into()], &every).await.unwrap();
	assert_eq!(of_apps, revision);
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
	assert!(store.of_apps(&[], &every).await.unwrap().0.is_empty());
	let (platform, _) = store.of_apps(&["gemini".into()], &["platform".into()]).await.unwrap();
	assert!(platform.iter().all(|kept| kept.named.scope == "platform") && platform.len() == 2);
	assert!(store.of_apps(&["gemini".into()], &["accounts".into()]).await.unwrap().0.is_empty());
	assert!(store.any().await.unwrap().is_some());
}
