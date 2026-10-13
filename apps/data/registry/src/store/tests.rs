use super::testing::{bare, store};
use super::{Kind, Set};
use crate::migrations;
use serde_json::{Value, json};

fn node(address: &str) -> Value {
	json!({ "address": address, "tier": "datacenter", "domain": "oci", "system": "debian" })
}

#[tokio::test]
async fn starts_empty_at_change_zero() {
	let Some(store) = store().await else { return };
	let nodes = store.nodes().await.unwrap();
	assert!(nodes.nodes.is_empty());
	assert_eq!(nodes.change, 0);
	assert!(store.changes(0, 100).await.unwrap().is_empty());
}

#[tokio::test]
async fn a_put_of_the_record_kept_writes_no_change() {
	let Some(store) = store().await else { return };
	let first = store.put("tyo", "test", node("100.64.0.1")).await.unwrap();
	assert!(first.is_some());
	// The same object, its keys in another order: jsonb compares what they hold.
	let same =
		json!({ "system": "debian", "domain": "oci", "tier": "datacenter", "address": "100.64.0.1" });
	assert_eq!(store.put("tyo", "test", same).await.unwrap(), None);
	assert_eq!(store.changes(0, 100).await.unwrap().len(), 1);
	assert_eq!(store.nodes().await.unwrap().change, first.unwrap());
}

#[tokio::test]
async fn a_changed_put_records_what_was_and_what_is() {
	let Some(store) = store().await else { return };
	let created = store.put("tyo", "alice", node("100.64.0.1")).await.unwrap().unwrap();
	let changed = store.put("tyo", "bob", node("100.64.0.2")).await.unwrap().unwrap();
	assert!(changed > created);

	let changes = store.changes(0, 100).await.unwrap();
	assert_eq!(changes.len(), 2);
	assert_eq!((changes[0].id, changes[0].by.as_str()), (created, "alice"));
	assert_eq!(changes[0].name, "tyo");
	assert_eq!(changes[0].before, None);
	assert_eq!(changes[0].after, Some(node("100.64.0.1")));
	assert_eq!((changes[1].id, changes[1].by.as_str()), (changed, "bob"));
	assert_eq!(changes[1].before, Some(node("100.64.0.1")));
	assert_eq!(changes[1].after, Some(node("100.64.0.2")));

	let nodes = store.nodes().await.unwrap();
	assert_eq!(nodes.nodes["tyo"], node("100.64.0.2"));
	assert_eq!(nodes.change, changed);
}

#[tokio::test]
async fn a_delete_records_what_was_and_nothing_after() {
	let Some(store) = store().await else { return };
	assert_eq!(store.delete("tyo", "test").await.unwrap(), None);
	store.put("tyo", "test", node("100.64.0.1")).await.unwrap();
	let deleted = store.delete("tyo", "carol").await.unwrap().unwrap();
	assert!(store.nodes().await.unwrap().nodes.is_empty());

	let changes = store.changes(0, 100).await.unwrap();
	let last = changes.last().unwrap();
	assert_eq!((last.id, last.by.as_str(), last.name.as_str()), (deleted, "carol", "tyo"));
	assert_eq!(last.before, Some(node("100.64.0.1")));
	assert_eq!(last.after, None);
	assert_eq!(store.delete("tyo", "test").await.unwrap(), None);
}

#[tokio::test]
async fn nodes_are_answered_in_name_order() {
	let Some(store) = store().await else { return };
	for name in ["tyo", "rdu", "sin"] {
		store.put(name, "test", node("100.64.0.1")).await.unwrap();
	}
	let names: Vec<String> = store.nodes().await.unwrap().nodes.keys().cloned().collect();
	assert_eq!(names, ["rdu", "sin", "tyo"]);
}

#[tokio::test]
async fn changes_page_oldest_first_after_an_id() {
	let Some(store) = store().await else { return };
	let mut ids = Vec::new();
	for (index, name) in ["aaa", "bbb", "ccc", "ddd", "eee"].into_iter().enumerate() {
		let address = format!("100.64.0.{index}");
		ids.push(store.put(name, "test", node(&address)).await.unwrap().unwrap());
	}

	let mut seen = Vec::new();
	let mut after = 0;
	loop {
		let page = store.changes(after, 2).await.unwrap();
		if page.is_empty() {
			break;
		}
		assert!(page.len() <= 2);
		after = page.last().unwrap().id;
		seen.extend(page.into_iter().map(|change| change.id));
	}
	assert_eq!(seen, ids);
	assert_eq!(store.changes(ids[2], 100).await.unwrap().len(), 2);
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
	assert_eq!(store.nodes().await.unwrap().change, id);
	assert_eq!(store.changes(0, 100).await.unwrap()[0].by, "old");
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
	let node_at = store.put("tyo", "test", node("100.64.0.1")).await.unwrap().unwrap();
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

	// What every relay reads moves with the nodes alone.
	assert_eq!(store.nodes().await.unwrap().change, node_at);
	let changes = store.changes(0, 100).await.unwrap();
	assert_eq!(changes.iter().map(|change| change.id).collect::<Vec<_>>(), [node_at]);
}

#[tokio::test]
async fn a_node_deleted_is_set_again_from_revision_zero() {
	let Some(store) = store().await else { return };
	let created = store.put("tyo", "test", node("100.64.0.1")).await.unwrap().unwrap();
	store.delete("tyo", "test").await.unwrap().unwrap();
	assert!(store.resource(Kind::Node, "tyo").await.unwrap().is_none());
	let stale = store.set(Kind::Node, "tyo", "test", created, node("100.64.0.2")).await.unwrap();
	assert_eq!(stale, Set::Stale);
	let again = store.set(Kind::Node, "tyo", "test", 0, node("100.64.0.2")).await.unwrap();
	assert!(matches!(again, Set::Changed(id) if id > created));
}
