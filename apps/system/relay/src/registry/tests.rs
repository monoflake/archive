use super::*;

// Nodes by their ids in infra's nodes.toml, which sort sin, gvx, tyo, rdu, buf.
const SIN: &str = "9d399312d1c3ddc4";
const GVX: &str = "a0a24bbaa0c0beba";
const TYO: &str = "d132b0f553e3dcc3";
const RDU: &str = "d62c82b23a858876";
const BUF: &str = "e38f403f033d87f7";

fn node(address: &str) -> Node {
	Node { address: address.into(), retired: false }
}

fn retired(address: &str) -> Node {
	Node { address: address.into(), retired: true }
}

fn peer(id: &str, address: &str) -> Peer {
	Peer { id: id.into(), address: address.into() }
}

fn held(peers: &[Peer]) -> BTreeMap<String, Peer> {
	peers.iter().map(|peer| (peer.id.clone(), peer.clone())).collect()
}

fn answer(nodes: &[(&str, Node)], revision: i64) -> Answer {
	let item = |(id, spec): &(&str, Node)| Item { id: (*id).to_owned(), spec: spec.clone() };
	Answer { items: nodes.iter().map(item).collect(), revision }
}

fn taken(nodes: &[(&str, Node)], revision: i64) -> Taken {
	let nodes = nodes.iter().map(|(id, node)| ((*id).to_owned(), node.clone())).collect();
	Taken { nodes, revision }
}

const THIS: (&str, Node) = (RDU, Node { address: String::new(), retired: false });

/// An answer as control gives it, with keys a relay does not read.
#[test]
fn reads_the_answer_as_control_gives_it() {
	let body = format!(
		r#"{{ "status": "success", "data": {{ "revision": 7, "items": [
		{{ "kind": "node", "id": "{BUF}", "name": null, "revision": 5,
			"spec": {{ "address": "100.64.0.2", "system": "alpine", "retired": true }} }},
		{{ "kind": "node", "id": "{TYO}", "name": null, "revision": 3,
			"spec": {{ "address": "100.64.0.1", "roles": ["core"], "tier": "datacenter" }} }} ] }} }}"#
	);
	let response::Envelope::Success { data } = serde_json::from_str(&body).unwrap() else {
		panic!("a success");
	};
	let read: Answer = data;
	let listed = [(BUF, retired("100.64.0.2")), (TYO, node("100.64.0.1"))];
	assert_eq!(read, answer(&listed, 7));
}

#[test]
fn control_is_asked_for_the_nodes_under_its_registry_scope() {
	assert_eq!(nodes(monoflake::CONTROL), format!("{}{NODES}", monoflake::CONTROL));
	assert_eq!(NODES, "/v1/registry/nodes");
}

#[test]
fn a_node_listed_and_not_retired_is_added_on_the_relays_port() {
	let listed = [THIS, (TYO, node("100.64.0.1")), (BUF, node("100.64.0.2"))];
	assert_eq!(
		next_peers(&BTreeMap::new(), &answer(&listed, 7), RDU),
		Some(vec![peer(TYO, "100.64.0.1:12012"), peer(BUF, "100.64.0.2:12012")])
	);
	// The only node listed is this one: taken, and nothing is held.
	assert_eq!(next_peers(&BTreeMap::new(), &answer(&[THIS], 8), RDU), Some(vec![]));
}

#[test]
fn a_node_listed_retired_is_dropped_and_one_listed_again_is_added() {
	let held = held(&[peer(TYO, "100.64.0.1:12012"), peer(BUF, "100.64.0.2:12012")]);
	let listed = [THIS, (BUF, retired("100.64.0.2")), (SIN, retired("100.64.0.4"))];
	assert_eq!(
		next_peers(&held, &answer(&listed, 9), RDU),
		Some(vec![peer(TYO, "100.64.0.1:12012")])
	);
	let back = [THIS, (BUF, node("100.64.0.2"))];
	let without_buf = BTreeMap::from([(TYO.to_owned(), peer(TYO, "100.64.0.1:12012"))]);
	let next = next_peers(&without_buf, &answer(&back, 10), RDU).unwrap();
	assert_eq!(next, [peer(TYO, "100.64.0.1:12012"), peer(BUF, "100.64.0.2:12012")]);
}

/// A write that drops a node by mistake disconnects nothing: a node leaves by `retired` alone.
#[test]
fn a_node_missing_from_an_answer_is_kept() {
	// `gvx` from RELAY_PEERS and `buf` from an earlier answer, neither listed now.
	let held = held(&[peer(GVX, "gvx.test:12012"), peer(BUF, "100.64.0.2:12012")]);
	let next = next_peers(&held, &answer(&[THIS, (TYO, node("100.64.0.1"))], 9), RDU);
	assert_eq!(
		next,
		Some(vec![
			peer(GVX, "gvx.test:12012"),
			peer(TYO, "100.64.0.1:12012"),
			peer(BUF, "100.64.0.2:12012"),
		])
	);
}

#[test]
fn an_answer_not_listing_this_node_is_not_taken_an_empty_one_included() {
	let held = held(&[peer(TYO, "100.64.0.1:12012")]);
	let others = answer(&[(TYO, retired("100.64.0.1"))], 7);
	assert_eq!(next_peers(&held, &others, RDU), None);
	assert_eq!(next_peers(&held, &answer(&[], 8), RDU), None);
	// This node by its name, as the registry listed it, is not this node.
	assert_eq!(next_peers(&held, &answer(&[("rdu", node("100.64.0.3"))], 9), RDU), None);
}

#[test]
fn what_is_taken_keeps_every_node_listed_before() {
	let before = taken(&[(TYO, node("100.64.0.1")), (BUF, node("100.64.0.2"))], 5);
	let folded = fold(Some(&before), &answer(&[THIS, (BUF, retired("100.64.0.9"))], 9));
	let after = taken(&[THIS, (TYO, node("100.64.0.1")), (BUF, retired("100.64.0.9"))], 9);
	assert_eq!(folded, after);
	assert_eq!(folded.retired(), BTreeSet::from([BUF.to_owned()]));
	assert_eq!(fold(None, &answer(&[THIS], 3)), taken(&[THIS], 3));
}

#[test]
fn what_was_taken_wins_over_relay_peers_and_a_retired_node_is_not_seeded() {
	let given = [peer(TYO, "tyo.test:12012"), peer(GVX, "gvx.test:12012"), peer(BUF, "b:1")];
	let listed = [THIS, (TYO, node("100.64.0.1")), (BUF, retired("100.64.0.2"))];
	assert_eq!(
		seeded(&given, Some(&taken(&listed, 7)), RDU),
		[peer(GVX, "gvx.test:12012"), peer(TYO, "100.64.0.1:12012")]
	);
	let alone = [peer(GVX, "gvx.test:12012"), peer(TYO, "tyo.test:12012"), peer(BUF, "b:1")];
	assert_eq!(seeded(&given, None, RDU), alone);
}

#[test]
fn a_peer_moved_is_stopped_and_dialed_again() {
	let held = held(&[peer(TYO, "a:12012"), peer(GVX, "b:12012"), peer(BUF, "c:12012")]);
	let next = [peer(TYO, "a:12012"), peer(BUF, "d:12012"), peer(SIN, "e:12012")];
	let (dialed, stopped) = changed(&held, &next);
	assert_eq!(dialed, [peer(BUF, "d:12012"), peer(SIN, "e:12012")]);
	assert_eq!(stopped, [GVX, BUF]);
}

#[test]
fn the_cache_is_read_back_and_a_missing_one_is_none() {
	let data = tempfile::tempdir().unwrap();
	assert!(cached(data.path()).unwrap().is_none());
	let kept = taken(&[(RDU, node("100.64.0.3")), (BUF, retired("100.64.0.2"))], 7);
	cache(data.path(), &kept).unwrap();
	assert_eq!(cached(data.path()).unwrap(), Some(kept));
	let written: serde_json::Value =
		serde_json::from_slice(&std::fs::read(data.path().join(FILE)).unwrap()).unwrap();
	assert_eq!(written["version"], VERSION);
	assert_eq!(written["revision"], 7);
	assert!(written["nodes"][RDU].get("retired").is_none());
	let newer = r#"{ "version": 4, "nodes": {}, "revision": 1 }"#;
	std::fs::write(data.path().join(FILE), newer).unwrap();
	assert!(matches!(cached(data.path()), Err(CacheError::Version(4))));
}

/// Versions 1 and 2 hold nodes by name; a relay on ids starts from `RELAY_PEERS` instead, and the
/// first answer it takes writes over them.
#[test]
fn a_cache_by_name_is_passed_over() {
	let data = tempfile::tempdir().unwrap();
	let one = r#"{ "version": 1, "change": 12, "nodes": {
		"rdu": { "address": "100.64.0.3", "core": true },
		"buf": { "address": "100.64.0.2" } } }"#;
	let two = r#"{ "version": 2, "revision": 4, "nodes": {
		"tyo": { "address": "100.64.0.1", "core": true } } }"#;
	for (version, file) in [(1, one), (2, two)] {
		std::fs::write(data.path().join(FILE), file).unwrap();
		assert!(matches!(cached(data.path()), Err(CacheError::Version(read)) if read == version));
	}

	let folded = fold(None, &answer(&[THIS, (BUF, node("100.64.0.2"))], 12));
	cache(data.path(), &folded).unwrap();
	assert_eq!(cached(data.path()).unwrap(), Some(folded));
}
