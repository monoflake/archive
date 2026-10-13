use super::*;

fn node(address: &str, core: bool) -> Node {
	Node { address: address.into(), core, retired: false }
}

fn retired(address: &str) -> Node {
	Node { address: address.into(), core: false, retired: true }
}

fn peer(name: &str, address: &str) -> Peer {
	Peer { name: name.into(), address: address.into() }
}

fn held(peers: &[Peer]) -> BTreeMap<String, Peer> {
	peers.iter().map(|peer| (peer.name.clone(), peer.clone())).collect()
}

fn answer(nodes: &[(&str, Node)], revision: i64) -> Answer {
	let item = |(name, spec): &(&str, Node)| Item { name: (*name).to_owned(), spec: spec.clone() };
	Answer { items: nodes.iter().map(item).collect(), revision }
}

fn taken(nodes: &[(&str, Node)], revision: i64) -> Taken {
	let nodes = nodes.iter().map(|(name, node)| ((*name).to_owned(), node.clone())).collect();
	Taken { nodes, revision }
}

const RDU: (&str, Node) = ("rdu", Node { address: String::new(), core: true, retired: false });

/// An answer as the registry gives it, with keys a relay does not read and `core` left out.
#[test]
fn reads_the_answer_as_the_registry_gives_it() {
	let body = r#"{ "status": "success", "data": { "revision": 7, "items": [
		{ "kind": "node", "name": "buf", "revision": 5,
			"spec": { "address": "100.64.0.2", "system": "alpine", "retired": true } },
		{ "kind": "node", "name": "tyo", "revision": 3,
			"spec": { "address": "100.64.0.1", "core": true, "tier": "datacenter" } } ] } }"#;
	let response::Envelope::Success { data } = serde_json::from_str(body).unwrap() else {
		panic!("a success");
	};
	let read: Answer = data;
	let listed = [("buf", retired("100.64.0.2")), ("tyo", node("100.64.0.1", true))];
	assert_eq!(read, answer(&listed, 7));
}

#[test]
fn a_node_listed_and_not_retired_is_added_on_the_relays_port() {
	let listed = [RDU, ("tyo", node("100.64.0.1", true)), ("buf", node("100.64.0.2", false))];
	assert_eq!(
		next_peers(&BTreeMap::new(), &answer(&listed, 7), "rdu"),
		Some(vec![peer("buf", "100.64.0.2:12012"), peer("tyo", "100.64.0.1:12012")])
	);
	// The only node listed is this one: taken, and nothing is held.
	assert_eq!(next_peers(&BTreeMap::new(), &answer(&[RDU], 8), "rdu"), Some(vec![]));
}

#[test]
fn a_node_listed_retired_is_dropped_and_one_listed_again_is_added() {
	let held = held(&[peer("tyo", "100.64.0.1:12012"), peer("buf", "100.64.0.2:12012")]);
	let listed = [RDU, ("buf", retired("100.64.0.2")), ("sin", retired("100.64.0.4"))];
	assert_eq!(
		next_peers(&held, &answer(&listed, 9), "rdu"),
		Some(vec![peer("tyo", "100.64.0.1:12012")])
	);
	let back = [RDU, ("buf", node("100.64.0.2", false))];
	let without_buf = BTreeMap::from([("tyo".to_owned(), peer("tyo", "100.64.0.1:12012"))]);
	let next = next_peers(&without_buf, &answer(&back, 10), "rdu").unwrap();
	assert_eq!(next, [peer("buf", "100.64.0.2:12012"), peer("tyo", "100.64.0.1:12012")]);
}

/// A write that drops a node by mistake disconnects nothing: a node leaves by `retired` alone.
#[test]
fn a_node_missing_from_an_answer_is_kept() {
	// `gvx` from RELAY_PEERS and `buf` from an earlier answer, neither listed now.
	let held = held(&[peer("gvx", "gvx.test:12012"), peer("buf", "100.64.0.2:12012")]);
	let next = next_peers(&held, &answer(&[RDU, ("tyo", node("100.64.0.1", true))], 9), "rdu");
	assert_eq!(
		next,
		Some(vec![
			peer("buf", "100.64.0.2:12012"),
			peer("gvx", "gvx.test:12012"),
			peer("tyo", "100.64.0.1:12012"),
		])
	);
}

#[test]
fn an_answer_not_listing_this_node_is_not_taken_an_empty_one_included() {
	let held = held(&[peer("tyo", "100.64.0.1:12012")]);
	let others = answer(&[("tyo", retired("100.64.0.1"))], 7);
	assert_eq!(next_peers(&held, &others, "rdu"), None);
	assert_eq!(next_peers(&held, &answer(&[], 8), "rdu"), None);
}

#[test]
fn what_is_taken_keeps_every_node_listed_before() {
	let before = taken(&[("tyo", node("100.64.0.1", true)), ("buf", node("100.64.0.2", false))], 5);
	let folded = fold(Some(&before), &answer(&[RDU, ("buf", retired("100.64.0.9"))], 9));
	let after = taken(&[RDU, ("tyo", node("100.64.0.1", true)), ("buf", retired("100.64.0.9"))], 9);
	assert_eq!(folded, after);
	assert_eq!(folded.retired(), BTreeSet::from(["buf".to_owned()]));
	assert_eq!(fold(None, &answer(&[RDU], 3)), taken(&[RDU], 3));
}

#[test]
fn what_was_taken_wins_over_relay_peers_and_a_retired_node_is_not_seeded() {
	let given = [peer("tyo", "tyo.test:12012"), peer("gvx", "gvx.test:12012"), peer("buf", "b:1")];
	let listed = [RDU, ("tyo", node("100.64.0.1", true)), ("buf", retired("100.64.0.2"))];
	assert_eq!(
		seeded(&given, Some(&taken(&listed, 7)), "rdu"),
		[peer("gvx", "gvx.test:12012"), peer("tyo", "100.64.0.1:12012")]
	);
	let alone = [peer("buf", "b:1"), peer("gvx", "gvx.test:12012"), peer("tyo", "tyo.test:12012")];
	assert_eq!(seeded(&given, None, "rdu"), alone);
}

#[test]
fn a_peer_moved_is_stopped_and_dialed_again() {
	let held = held(&[peer("tyo", "a:12012"), peer("gvx", "b:12012"), peer("buf", "c:12012")]);
	let next = [peer("tyo", "a:12012"), peer("buf", "d:12012"), peer("sin", "e:12012")];
	let (dialed, stopped) = changed(&held, &next);
	assert_eq!(dialed, [peer("buf", "d:12012"), peer("sin", "e:12012")]);
	assert_eq!(stopped, ["buf", "gvx"]);
}

#[test]
fn the_core_is_asked_first_then_every_other_peer() {
	let held = held(&[peer("tyo", "100.64.0.1:12012"), peer("buf", "100.64.0.2:12012")]);
	let mut gone = retired("100.64.0.5");
	gone.core = true;
	let listed = [
		("tyo", node("100.64.0.1", true)),
		("sin", node("100.64.0.4", true)),
		("buf", node("100.64.0.2", false)),
		("old", gone),
	];
	let taken = taken(&listed, 7);
	let asked = |taken| -> Vec<(String, String)> { candidates(taken, &held) };
	let pairs = |pairs: &[(&str, &str)]| -> Vec<(String, String)> {
		pairs.iter().map(|(a, b)| ((*a).to_owned(), (*b).to_owned())).collect()
	};
	assert_eq!(
		asked(Some(&taken)),
		pairs(&[("sin", "100.64.0.4"), ("tyo", "100.64.0.1"), ("buf", "100.64.0.2")])
	);
	// Before any answer is taken, every peer, from `RELAY_PEERS`.
	assert_eq!(asked(None), pairs(&[("buf", "100.64.0.2"), ("tyo", "100.64.0.1")]));
}

#[test]
fn the_cache_is_read_back_and_a_missing_one_is_none() {
	let data = tempfile::tempdir().unwrap();
	assert!(cached(data.path()).unwrap().is_none());
	let kept = taken(&[("rdu", node("100.64.0.3", true)), ("buf", retired("100.64.0.2"))], 7);
	cache(data.path(), &kept).unwrap();
	assert_eq!(cached(data.path()).unwrap(), Some(kept));
	let written: serde_json::Value =
		serde_json::from_slice(&std::fs::read(data.path().join(FILE)).unwrap()).unwrap();
	assert_eq!(written["version"], VERSION);
	assert_eq!(written["revision"], 7);
	assert!(written["nodes"]["buf"].get("core").is_none());
	assert!(written["nodes"]["rdu"].get("retired").is_none());
	let newer = r#"{ "version": 3, "nodes": {}, "revision": 1 }"#;
	std::fs::write(data.path().join(FILE), newer).unwrap();
	assert!(matches!(cached(data.path()), Err(CacheError::Version(3))));
}

/// A relay restarted on this image keeps the peers its version-1 file held, and the first answer
/// it takes writes version 2.
#[test]
fn a_version_one_cache_is_carried_over() {
	let data = tempfile::tempdir().unwrap();
	let one = r#"{ "version": 1, "change": 12, "nodes": {
		"rdu": { "address": "100.64.0.3", "core": true },
		"buf": { "address": "100.64.0.2" } } }"#;
	std::fs::write(data.path().join(FILE), one).unwrap();
	let carried = cached(data.path()).unwrap().unwrap();
	let listed = [("rdu", node("100.64.0.3", true)), ("buf", node("100.64.0.2", false))];
	assert_eq!(carried, taken(&listed, 0));
	assert_eq!(seeded(&[], Some(&carried), "rdu"), [peer("buf", "100.64.0.2:12012")]);

	let folded = fold(Some(&carried), &answer(&[RDU], 12));
	cache(data.path(), &folded).unwrap();
	let written: serde_json::Value =
		serde_json::from_slice(&std::fs::read(data.path().join(FILE)).unwrap()).unwrap();
	assert_eq!(written["version"], 2);
	assert_eq!(cached(data.path()).unwrap().unwrap().nodes["buf"], node("100.64.0.2", false));
}
