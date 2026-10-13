use super::testing::{bare, store};
use super::{Kind, Set, Store};
use crate::migrations;
use serde_json::{Value, json};

fn node(address: &str) -> Value {
	json!({ "address": address, "tier": "datacenter", "domain": "oci", "system": "debian" })
}

/// One row of `changes`, the history: who set `name` of `kind` from `before` to `after`.
#[derive(Debug, PartialEq)]
struct Row {
	id: i64,
	kind: String,
	by: String,
	name: String,
	before: Option<Value>,
	after: Option<Value>,
}

/// Every row of `changes`, oldest first. No route reads it; it is kept as the history.
async fn history(store: &Store) -> Vec<Row> {
	let rows = store
		.pool
		.get()
		.await
		.unwrap()
		.query("SELECT id, kind, by, name, before, after FROM changes ORDER BY id", &[])
		.await
		.unwrap();
	let row = |row: tokio_postgres::Row| Row {
		id: row.get(0),
		kind: row.get(1),
		by: row.get(2),
		name: row.get(3),
		before: row.get(4),
		after: row.get(5),
	};
	rows.into_iter().map(row).collect()
}

/// A set that must append a change, answering its id.
async fn changed(store: &Store, kind: Kind, name: &str, by: &str, at: i64, spec: Value) -> i64 {
	match store.set(kind, name, by, at, spec).await.unwrap() {
		Set::Changed(id) => id,
		other => panic!("{name}: {other:?}"),
	}
}

#[tokio::test]
async fn starts_empty_at_revision_zero() {
	let Some(store) = store().await else { return };
	let nodes = store.resources(Kind::Node).await.unwrap();
	assert!(nodes.items.is_empty());
	assert_eq!(nodes.revision, 0);
	assert!(history(&store).await.is_empty());
}

#[tokio::test]
async fn a_set_of_the_spec_kept_writes_no_change() {
	let Some(store) = store().await else { return };
	let first = changed(&store, Kind::Node, "tyo", "test", 0, node("100.64.0.1")).await;
	// The same object, its keys in another order: jsonb compares what they hold.
	let same =
		json!({ "system": "debian", "domain": "oci", "tier": "datacenter", "address": "100.64.0.1" });
	assert_eq!(store.set(Kind::Node, "tyo", "test", first, same).await.unwrap(), Set::Unchanged);
	assert_eq!(history(&store).await.len(), 1);
	assert_eq!(store.resources(Kind::Node).await.unwrap().revision, first);
}

#[tokio::test]
async fn a_change_records_who_what_was_and_what_is() {
	let Some(store) = store().await else { return };
	let created = changed(&store, Kind::Node, "tyo", "alice", 0, node("100.64.0.1")).await;
	let moved = changed(&store, Kind::Node, "tyo", "bob", created, node("100.64.0.2")).await;
	assert!(moved > created);

	let row = |id, by: &str, before, after| Row {
		id,
		kind: "node".into(),
		by: by.into(),
		name: "tyo".into(),
		before,
		after: Some(after),
	};
	assert_eq!(
		history(&store).await,
		[
			row(created, "alice", None, node("100.64.0.1")),
			row(moved, "bob", Some(node("100.64.0.1")), node("100.64.0.2")),
		]
	);
	let tyo = store.resource(Kind::Node, "tyo").await.unwrap().unwrap();
	assert_eq!((tyo.revision, tyo.spec), (moved, node("100.64.0.2")));
}

#[tokio::test]
async fn nodes_are_answered_in_name_order() {
	let Some(store) = store().await else { return };
	for name in ["tyo", "rdu", "sin"] {
		changed(&store, Kind::Node, name, "test", 0, node("100.64.0.1")).await;
	}
	let items = store.resources(Kind::Node).await.unwrap().items;
	let names: Vec<&str> = items.iter().map(|node| node.name.as_str()).collect();
	assert_eq!(names, ["rdu", "sin", "tyo"]);
}

fn app(placements: &[&str]) -> Value {
	json!({ "placements": placements })
}

/// What a release before the second migration left: a node, its change, and no `kind` anywhere.
#[tokio::test]
async fn the_second_migration_keeps_every_change_as_a_node_s() {
	let Some(store) = bare().await else { return };
	let (mut client, connection) = store.config.connect(tokio_postgres::NoTls).await.unwrap();
	tokio::spawn(connection);
	assert_eq!(migrations::apply(&mut client, &migrations::MIGRATIONS[..1]).await.unwrap(), [1]);
	let id: i64 = client
		.query_one(
			"WITH node AS (INSERT INTO nodes (name, record) VALUES ('tyo', $1))
			INSERT INTO changes (by, name, after) VALUES ('old', 'tyo', $1) RETURNING id",
			&[&node("100.64.0.1")],
		)
		.await
		.unwrap()
		.get(0);

	assert_eq!(store.migrate().await.unwrap(), [2]);
	let kind: String =
		client.query_one("SELECT kind FROM changes WHERE id = $1", &[&id]).await.unwrap().get(0);
	assert_eq!(kind, "node");
	let tyo = store.resource(Kind::Node, "tyo").await.unwrap().unwrap();
	assert_eq!((tyo.revision, tyo.spec), (id, node("100.64.0.1")));
	assert_eq!(history(&store).await[0].by, "old");
	assert!(store.migrate().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_set_is_held_to_the_revision_read() {
	let Some(store) = store().await else { return };
	assert!(store.resource(Kind::App, "web").await.unwrap().is_none());
	assert_eq!(store.set(Kind::App, "web", "test", 1, app(&["tyo"])).await.unwrap(), Set::Stale);
	let created = store.set(Kind::App, "web", "alice", 0, app(&["tyo"])).await.unwrap();
	let Set::Changed(created) = created else { panic!("not created") };
	assert_eq!(store.set(Kind::App, "web", "bob", 0, app(&["sin"])).await.unwrap(), Set::Stale);
	let same = store.set(Kind::App, "web", "bob", created, app(&["tyo"])).await.unwrap();
	assert_eq!(same, Set::Unchanged);
	let moved = store.set(Kind::App, "web", "bob", created, app(&["sin"])).await.unwrap();
	let Set::Changed(moved) = moved else { panic!("not moved") };
	assert!(moved > created);
	let stale = store.set(Kind::App, "web", "carol", created, app(&["rdu"])).await.unwrap();
	assert_eq!(stale, Set::Stale);

	let web = store.resource(Kind::App, "web").await.unwrap().unwrap();
	assert_eq!((web.kind, web.revision, web.spec), (Kind::App, moved, app(&["sin"])));
}

#[tokio::test]
async fn a_kind_s_revisions_are_its_own_and_a_list_s_the_newest_of_any() {
	let Some(store) = store().await else { return };
	let node_at = changed(&store, Kind::Node, "tyo", "test", 0, node("100.64.0.1")).await;
	// An app named as a node is, so a revision read by name alone would mix the two.
	let app_at = store.set(Kind::App, "tyo", "test", 0, app(&["tyo"])).await.unwrap();
	let Set::Changed(app_at) = app_at else { panic!("not created") };
	store.set(Kind::App, "api", "test", 0, app(&[])).await.unwrap();

	let nodes = store.resources(Kind::Node).await.unwrap();
	let apps = store.resources(Kind::App).await.unwrap();
	assert_eq!(nodes.items.len(), 1);
	assert_eq!(nodes.items[0].revision, node_at);
	let names: Vec<&str> = apps.items.iter().map(|app| app.name.as_str()).collect();
	assert_eq!(names, ["api", "tyo"]);
	assert_eq!(apps.items[1].revision, app_at);
	assert_eq!(nodes.revision, apps.revision);
	assert!(apps.revision > app_at);
	let kinds: Vec<String> = history(&store).await.into_iter().map(|row| row.kind).collect();
	assert_eq!(kinds, ["node", "app", "app"]);
}

/// The old `DELETE /nodes` removed a node's row and appended the change; that history stays.
#[tokio::test]
async fn a_node_deleted_is_set_again_from_revision_zero() {
	let Some(store) = store().await else { return };
	let created = changed(&store, Kind::Node, "tyo", "test", 0, node("100.64.0.1")).await;
	let client = store.pool.get().await.unwrap();
	client
		.execute(
			"WITH gone AS (DELETE FROM nodes WHERE name = 'tyo' RETURNING record)
			INSERT INTO changes (kind, by, name, before) SELECT 'node', 'old', 'tyo', record FROM gone",
			&[],
		)
		.await
		.unwrap();
	drop(client);
	assert!(store.resource(Kind::Node, "tyo").await.unwrap().is_none());
	let stale = store.set(Kind::Node, "tyo", "test", created, node("100.64.0.2")).await.unwrap();
	assert_eq!(stale, Set::Stale);
	let again = store.set(Kind::Node, "tyo", "test", 0, node("100.64.0.2")).await.unwrap();
	assert!(matches!(again, Set::Changed(id) if id > created));
}
