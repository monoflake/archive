use super::testing::{bare, store};
use super::{Key, Kind, Set, Store};
use crate::{kinds, migrations};
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
		Set::Changed(written) => written.revision,
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
	let tyo = store.resource(Kind::Node, Key::Name("tyo")).await.unwrap().unwrap();
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

	assert_eq!(store.migrate().await.unwrap(), [2, 3, 4]);
	let kind: String =
		client.query_one("SELECT kind FROM changes WHERE id = $1", &[&id]).await.unwrap().get(0);
	assert_eq!(kind, "node");
	let tyo = store.resource(Kind::Node, Key::Name("tyo")).await.unwrap().unwrap();
	assert_eq!((tyo.revision, tyo.spec), (id, node("100.64.0.1")));
	assert_eq!(history(&store).await[0].by, "old");
	assert!(store.migrate().await.unwrap().is_empty());
}

#[tokio::test]
async fn a_set_is_held_to_the_revision_read() {
	let Some(store) = store().await else { return };
	assert!(store.resource(Kind::App, Key::Name("web")).await.unwrap().is_none());
	assert_eq!(store.set(Kind::App, "web", "test", 1, app(&["tyo"])).await.unwrap(), Set::Stale);
	let created = store.set(Kind::App, "web", "alice", 0, app(&["tyo"])).await.unwrap();
	let Set::Changed(created) = created else { panic!("not created") };
	let created = created.revision;
	assert_eq!(store.set(Kind::App, "web", "bob", 0, app(&["sin"])).await.unwrap(), Set::Stale);
	let same = store.set(Kind::App, "web", "bob", created, app(&["tyo"])).await.unwrap();
	assert_eq!(same, Set::Unchanged);
	let moved = store.set(Kind::App, "web", "bob", created, app(&["sin"])).await.unwrap();
	let Set::Changed(moved) = moved else { panic!("not moved") };
	let moved = moved.revision;
	assert!(moved > created);
	let stale = store.set(Kind::App, "web", "carol", created, app(&["rdu"])).await.unwrap();
	assert_eq!(stale, Set::Stale);

	let web = store.resource(Kind::App, Key::Name("web")).await.unwrap().unwrap();
	assert_eq!((web.kind, web.revision, web.spec), (Kind::App, moved, app(&["sin"])));
}

#[tokio::test]
async fn a_kind_s_revisions_are_its_own_and_a_list_s_the_newest_of_any() {
	let Some(store) = store().await else { return };
	let node_at = changed(&store, Kind::Node, "tyo", "test", 0, node("100.64.0.1")).await;
	// An app named as a node is, so a revision read by name alone would mix the two.
	let app_at = store.set(Kind::App, "tyo", "test", 0, app(&["tyo"])).await.unwrap();
	let Set::Changed(app_at) = app_at else { panic!("not created") };
	let app_at = app_at.revision;
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
	assert!(store.resource(Kind::Node, Key::Name("tyo")).await.unwrap().is_none());
	let stale = store.set(Kind::Node, "tyo", "test", created, node("100.64.0.2")).await.unwrap();
	assert_eq!(stale, Set::Stale);
	let again = store.set(Kind::Node, "tyo", "test", 0, node("100.64.0.2")).await.unwrap();
	assert!(matches!(again, Set::Changed(written) if written.revision > created));
}

/// The resource and the task a history row keeps, by the change's id.
async fn kept(store: &Store, change: i64) -> (Option<String>, Option<String>) {
	let client = store.pool.get().await.unwrap();
	let row =
		client.query_one("SELECT resource, task FROM changes WHERE id = $1", &[&change]).await.unwrap();
	(row.get(0), row.get(1))
}

/// What a release before the third migration left: two nodes and an app, and a node deleted by the
/// old `DELETE` and written again, its first life's rows parted from its second's.
#[tokio::test]
async fn the_third_migration_gives_every_resource_an_id_and_every_row_its_resource() {
	let Some(store) = bare().await else { return };
	let (mut client, connection) = store.config.connect(tokio_postgres::NoTls).await.unwrap();
	tokio::spawn(connection);
	assert_eq!(migrations::apply(&mut client, &migrations::MIGRATIONS[..2]).await.unwrap(), [1, 2]);
	let wrote = |sql: &'static str| {
		let client = &client;
		async move { client.query_one(sql, &[]).await.unwrap().get::<_, i64>(0) }
	};
	let tyo = wrote(
		r#"WITH n AS (INSERT INTO nodes (name, record) VALUES ('tyo', '{"a": 1}'))
		INSERT INTO changes (by, name, after) VALUES ('old', 'tyo', '{"a": 1}') RETURNING id"#,
	)
	.await;
	let web = wrote(
		r#"WITH a AS (INSERT INTO apps (name, spec) VALUES ('web', '{"placements": []}'))
		INSERT INTO changes (kind, by, name, after) VALUES ('app', 'old', 'web', '{"placements": []}')
		RETURNING id"#,
	)
	.await;
	let first_life = wrote(
		r#"INSERT INTO changes (by, name, after) VALUES ('old', 'gvx', '{"a": 1}') RETURNING id"#,
	)
	.await;
	let deleted = wrote(
		r#"INSERT INTO changes (by, name, before) VALUES ('old', 'gvx', '{"a": 1}') RETURNING id"#,
	)
	.await;
	let second_life = wrote(
		r#"WITH n AS (INSERT INTO nodes (name, record) VALUES ('gvx', '{"a": 2}'))
		INSERT INTO changes (by, name, after) VALUES ('old', 'gvx', '{"a": 2}') RETURNING id"#,
	)
	.await;

	assert_eq!(store.migrate().await.unwrap(), [3, 4]);
	let id = |kind, name: &'static str| {
		let store = &store;
		async move { store.resource(kind, Key::Name(name)).await.unwrap().unwrap().id }
	};
	let (tyo_id, gvx_id, web_id) =
		(id(Kind::Node, "tyo").await, id(Kind::Node, "gvx").await, id(Kind::App, "web").await);
	for id in [&tyo_id, &gvx_id, &web_id] {
		assert!(kinds::is_id(id), "{id}");
	}
	assert_ne!(tyo_id, gvx_id, "each row is given an id of its own");
	assert_eq!(kept(&store, tyo).await, (Some(tyo_id.clone()), None));
	assert_eq!(kept(&store, web).await, (Some(web_id), None));
	assert_eq!(kept(&store, second_life).await, (Some(gvx_id), None));
	for row in [first_life, deleted] {
		assert_eq!(kept(&store, row).await, (None, None), "a life a delete ended has no resource");
	}

	// A release before this one inserts no id; the default gives the row one.
	client
		.execute(r#"INSERT INTO nodes (name, record) VALUES ('sin', '{"a": 3}')"#, &[])
		.await
		.unwrap();
	assert!(kinds::is_id(&id(Kind::Node, "sin").await));
	assert!(store.migrate().await.unwrap().is_empty());
	assert_eq!(id(Kind::Node, "tyo").await, tyo_id, "a migration run again changes no id");
}

#[tokio::test]
async fn a_write_keeps_its_resource_s_id_and_its_task_in_the_history() {
	let Some(store) = store().await else { return };
	let Set::Changed(created) =
		store.set(Kind::Node, "tyo", "test", 0, node("100.64.0.1")).await.unwrap()
	else {
		panic!("not created");
	};
	assert!(kinds::is_id(&created.id) && kinds::is_id(&created.task));
	let Set::Changed(moved) =
		store.set(Kind::Node, "tyo", "test", created.revision, node("100.64.0.2")).await.unwrap()
	else {
		panic!("not moved");
	};
	assert_eq!(moved.id, created.id, "an id is never changed");
	assert_ne!(moved.task, created.task, "each write is a task of its own");
	assert!(moved.at >= created.at);
	assert_eq!(kept(&store, created.revision).await, (Some(created.id.clone()), Some(created.task)));
	assert_eq!(kept(&store, moved.revision).await, (Some(created.id.clone()), Some(moved.task)));

	let by_id = store.resource(Kind::Node, Key::Id(&created.id)).await.unwrap();
	let by_name = store.resource(Kind::Node, Key::Name("tyo")).await.unwrap();
	assert_eq!(by_id, by_name);
	assert_eq!(by_id.unwrap().id, created.id);
	assert!(store.resource(Kind::App, Key::Id(&created.id)).await.unwrap().is_none());
	assert!(store.resource(Kind::Node, Key::Id(&"0".repeat(16))).await.unwrap().is_none());
}

#[test]
fn a_key_of_an_id_s_shape_is_an_id_and_any_other_a_name() {
	let id = "0123456789abcdef";
	assert_eq!(Key::of(id), Key::Id(id));
	let longer = format!("{id}{id}");
	for name in ["tyo", "web", &id[1..], &id.to_uppercase(), "0123456789abcdeg", &longer] {
		assert_eq!(Key::of(name), Key::Name(name), "{name}");
	}
}

/// `registry_id()` replaced, in this test's schema, by one that answers `ids` in turn.
async fn drawing(store: &Store, ids: &[&str]) {
	let client = store.pool.get().await.unwrap();
	let listed = ids.iter().map(|id| format!("'{id}'")).collect::<Vec<_>>().join(", ");
	client
		.batch_execute(&format!(
			"CREATE SEQUENCE drawn;
			CREATE OR REPLACE FUNCTION registry_id() RETURNS text LANGUAGE sql VOLATILE AS $$
				SELECT (ARRAY[{listed}])[nextval('drawn')]
			$$;"
		))
		.await
		.unwrap();
}

/// An id drawn twice is drawn again: a resource's against its kind's, a task's against every task.
#[tokio::test]
async fn an_id_already_held_is_drawn_again() {
	let Some(store) = store().await else { return };
	let first = changed(&store, Kind::Node, "tyo", "test", 0, node("100.64.0.1")).await;
	let held = store.resource(Kind::Node, Key::Name("tyo")).await.unwrap().unwrap().id;
	let (_, task) = kept(&store, first).await;
	let task = task.unwrap();
	// The node's id taken, then a fresh one for it; the first task's taken, then a fresh one.
	drawing(&store, &[&held, "00000000000000a1", &task, "00000000000000b2"]).await;
	let Set::Changed(written) =
		store.set(Kind::Node, "sin", "test", 0, node("100.64.0.2")).await.unwrap()
	else {
		panic!("not created");
	};
	assert_eq!(
		(written.id.as_str(), written.task.as_str()),
		("00000000000000a1", "00000000000000b2")
	);
}

/// Past `DRAWS` collisions the write fails, a fault rather than a write, and nothing is kept.
#[tokio::test]
async fn ids_that_keep_colliding_fail_the_write() {
	let Some(store) = store().await else { return };
	changed(&store, Kind::Node, "tyo", "test", 0, node("100.64.0.1")).await;
	let held = store.resource(Kind::Node, Key::Name("tyo")).await.unwrap().unwrap().id;
	drawing(&store, &[held.as_str(); super::DRAWS]).await;
	let failed = store.set(Kind::Node, "sin", "test", 0, node("100.64.0.2")).await;
	assert!(failed.unwrap_err().to_string().contains("each taken already"));
	assert!(store.resource(Kind::Node, Key::Name("sin")).await.unwrap().is_none());
	assert_eq!(history(&store).await.len(), 1);
}

/// What `registry_id()` makes: 16 lowercase hexadecimal digits, none of them the version or variant
/// nibble a UUID fixes, so every one of the 64 bits is drawn.
#[tokio::test]
async fn an_id_is_sixteen_digits_every_one_drawn() {
	let Some(store) = store().await else { return };
	let client = store.pool.get().await.unwrap();
	let rows = client.query("SELECT registry_id() FROM generate_series(1, 2000)", &[]).await.unwrap();
	let ids: Vec<String> = rows.iter().map(|row| row.get(0)).collect();
	assert!(ids.iter().all(|id| kinds::is_id(id)), "{:?}", &ids[..3]);
	let distinct: std::collections::BTreeSet<&String> = ids.iter().collect();
	assert_eq!(distinct.len(), ids.len());
	for place in 0..16 {
		let seen: std::collections::BTreeSet<u8> = ids.iter().map(|id| id.as_bytes()[place]).collect();
		assert_eq!(seen.len(), 16, "digit {place} takes only {seen:?}");
	}
}

/// An account and a domain are kept as every resource is, each with an id and a history row.
#[tokio::test]
async fn an_account_and_a_domain_are_held_as_every_resource_is() {
	let Some(store) = store().await else { return };
	let account =
		changed(&store, Kind::Account, "oci", "test", 0, json!({ "provider": "oracle" })).await;
	let domain = changed(&store, Kind::Domain, "oci", "test", 0, json!({})).await;
	let accounts = store.resources(Kind::Account).await.unwrap();
	let domains = store.resources(Kind::Domain).await.unwrap();
	assert_eq!((accounts.items.len(), domains.items.len()), (1, 1));
	assert_eq!((accounts.items[0].revision, domains.items[0].revision), (account, domain));
	assert!(kinds::is_id(&accounts.items[0].id) && kinds::is_id(&domains.items[0].id));
	assert_ne!(accounts.items[0].id, domains.items[0].id);
	assert!(store.resources(Kind::Node).await.unwrap().items.is_empty(), "one name, kinds apart");
	let kinds: Vec<String> = history(&store).await.into_iter().map(|row| row.kind).collect();
	assert_eq!(kinds, ["account", "domain"]);
}
