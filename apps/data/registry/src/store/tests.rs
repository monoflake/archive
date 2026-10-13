use super::testing::store;
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
