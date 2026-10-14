//! Syncing by digest: deltas, a want, and a digest that differs.

use super::*;

/// tyo's hello, syncing by digest, holding `versions`.
fn synced_hello(versions: Versions) -> Received {
	let hello = Message::Hello {
		version: VERSION,
		node: "tyo".into(),
		versions,
		runs: None,
		minutes: None,
		beats: Some(true),
		watch: None,
		zstd: None,
		sync: Some(DIGEST.into()),
	};
	Received::Text(serde_json::to_string(&hello).unwrap())
}

fn text(message: &Message) -> Received {
	Received::Text(serde_json::to_string(message).unwrap())
}

fn carried(version: u64, image: &str) -> Carried {
	let snapshot = serde_json::json!({ "apps": [{ "name": "geo", "image": image },
		{ "name": "apt", "image": "apt:1" }] });
	Carried { version, snapshot: Arc::new(snapshot) }
}

/// What a conversation with a synced tyo holding `versions` sends, fed `then` after the hello,
/// with `during` done once it is under way.
async fn synced(
	relay: &Arc<Relay>,
	versions: Versions,
	then: Vec<Received>,
	during: impl FnOnce(),
	until: impl Fn(&[Message]) -> bool,
) -> Vec<Message> {
	let (feed, incoming) = mpsc::unbounded_channel();
	feed.send(synced_hello(versions)).unwrap();
	for one in then {
		feed.send(one).unwrap();
	}
	let sent = Arc::new(Mutex::new(Vec::new()));
	let fake = Heard::new(Fake { incoming, sent: sent.clone(), echo: None });
	let conversing = tokio::spawn(converse(relay.clone(), Some("tyo"), fake));
	tokio::time::sleep(Duration::from_millis(20)).await;
	during();
	let waited = async {
		while !until(&sent.lock().unwrap()) {
			tokio::time::sleep(Duration::from_millis(10)).await;
		}
	};
	tokio::time::timeout(Duration::from_secs(5), waited).await.expect("what was waited for");
	drop(feed);
	conversing.await.unwrap().unwrap();
	sent.lock().unwrap().clone()
}

#[tokio::test]
async fn a_synced_neighbor_is_sent_this_nodes_change_as_a_delta_from_what_it_holds() {
	let relay = relay("rdu");
	relay.take("rdu", carried(10, "geo:1"), None);
	let sent = synced(
		&relay,
		Versions::from([("rdu".into(), 10)]),
		vec![],
		|| {
			relay.take("rdu", carried(20, "geo:2"), None);
		},
		|sent| sent.iter().any(|one| matches!(one, Message::Delta { .. })),
	)
	.await;
	let delta = sent.iter().find(|one| matches!(one, Message::Delta { .. })).unwrap();
	let Message::Delta { node, base, version, digest, changes } = delta else { unreachable!() };
	assert_eq!((node.as_str(), *base, *version), ("rdu", 10, 20));
	let rows = changes.apps.as_ref().unwrap();
	assert_eq!(rows.set, [serde_json::json!({ "name": "geo", "image": "geo:2" })]);
	let applied = crate::delta::apply(&carried(10, "geo:1").snapshot, changes).unwrap();
	assert_eq!(*digest, hex(crate::delta::digest(&applied)));
	assert!(!sent.iter().any(|one| matches!(one, Message::Node { .. })), "nothing whole");
}

#[tokio::test]
async fn a_delta_that_does_not_apply_is_asked_for_whole_and_a_want_is_answered_whole() {
	let relay = relay("rdu");
	relay.take("buf", carried(3, "geo:1"), None);
	let changes = crate::delta::diff(&carried(3, "geo:1").snapshot, &carried(4, "geo:2").snapshot);
	let delta = |base: u64, digest: String| Message::Delta {
		node: "buf".into(),
		base,
		version: 4,
		digest,
		changes: changes.clone(),
	};
	let right = hex(crate::delta::digest(&carried(4, "geo:2").snapshot));
	let then = vec![
		// From a base this relay does not hold, and with a digest its result does not read.
		text(&delta(2, right.clone())),
		text(&delta(3, "0000000000000000".into())),
		text(&Message::Want { node: "buf".into() }),
	];
	let wanted = |sent: &[Message]| {
		let wants = sent.iter().filter(|one| matches!(one, Message::Want { .. })).count();
		let whole = sent.iter().any(|one| matches!(one, Message::Node { node, .. } if node == "buf"));
		wants == 2 && whole
	};
	let versions = Versions::from([("buf".into(), 3)]);
	synced(&relay, versions, then, || {}, wanted).await;
	assert_eq!(relay.versions()["buf"], 3);
	// The right one is taken, and only from the base held.
	let sent = synced(&relay, Versions::new(), vec![text(&delta(3, right))], || {}, |_| true).await;
	assert!(!sent.iter().any(|one| matches!(one, Message::Want { .. })));
	tokio::time::sleep(Duration::from_millis(20)).await;
	assert_eq!(relay.versions()["buf"], 4);
}

#[tokio::test]
async fn a_sync_saying_another_digest_at_the_same_version_is_answered_whole() {
	let relay = relay("rdu");
	relay.take("buf", carried(3, "geo:1"), None);
	let held = hex(relay.lines()["buf"].1);
	let sync = |digest: String| Message::Sync {
		nodes: BTreeMap::from([("buf".into(), Line(3, digest, 0))]),
		runs: Versions::new(),
		minutes: Versions::new(),
	};
	let versions = Versions::from([("buf".into(), 3)]);
	let whole = |sent: &[Message]| {
		sent.iter().any(|one| matches!(one, Message::Node { node, .. } if node == "buf"))
	};
	// The same digest wants nothing; another gets the node whole.
	let same = synced(&relay, versions.clone(), vec![text(&sync(held))], || {}, |_| true).await;
	assert!(!whole(&same));
	synced(&relay, versions, vec![text(&sync("ffffffffffffffff".into()))], || {}, whole).await;
}
