use super::neighbors::current;
use super::*;

fn relay(node: &str) -> Arc<Relay> {
	Relay::new(node.into(), "s3cret".into(), BTreeSet::new(), Store::memory().unwrap()).unwrap()
}

#[test]
fn one_socket_to_a_neighbor_speaks_until_it_is_hushed() {
	let relay = relay("d62c82b23a858876");
	let (first, second) = (relay.conversation(), relay.conversation());
	assert!(relay.speaks("d132b0f553e3dcc3", first));
	assert!(!relay.speaks("d132b0f553e3dcc3", second));
	assert!(relay.speaks("e38f403f033d87f7", second));
	relay.hush("d132b0f553e3dcc3", second);
	assert!(relay.speaks("d132b0f553e3dcc3", first));
	relay.hush("d132b0f553e3dcc3", first);
	assert!(relay.speaks("d132b0f553e3dcc3", second));
}

#[test]
fn a_round_trip_is_in_milliseconds_and_left_out_once_it_is_old() {
	let now = Instant::now();
	let timed = BTreeMap::from([
		("d132b0f553e3dcc3".into(), (Duration::from_micros(151_234), now - Duration::from_secs(1))),
		("e38f403f033d87f7".into(), (Duration::from_micros(460), now)),
		("a0a24bbaa0c0beba".into(), (Duration::from_millis(90), now - CURRENT)),
	]);
	assert_eq!(
		current(&timed, now),
		RoundTrips::from([("d132b0f553e3dcc3".into(), 0.1512), ("e38f403f033d87f7".into(), 0.0005)])
	);
}

#[test]
fn the_kernels_round_trip_stands_over_the_pings_and_the_pings_where_it_has_none() {
	let relay = relay("d62c82b23a858876");
	relay.timed("d132b0f553e3dcc3", Duration::from_millis(137));
	relay.timed("0196b5a8439abb66", Duration::from_millis(1234));
	relay.measured("d132b0f553e3dcc3", Duration::from_micros(69_250));
	let round_trips = relay.round_trips();
	assert_eq!(
		round_trips,
		RoundTrips::from([("d132b0f553e3dcc3".into(), 0.0693), ("0196b5a8439abb66".into(), 1.234)])
	);
}

#[test]
fn the_latest_round_trip_replaces_the_last() {
	let relay = relay("d62c82b23a858876");
	assert!(relay.round_trips().is_empty());
	relay.timed("d132b0f553e3dcc3", Duration::from_millis(150));
	relay.timed("d132b0f553e3dcc3", Duration::from_millis(149));
	assert_eq!(relay.round_trips(), RoundTrips::from([("d132b0f553e3dcc3".into(), 0.149)]));
}

#[test]
fn a_neighbors_word_on_this_node_is_not_taken() {
	let relay = relay("d62c82b23a858876");
	let carried = Carried { version: 9, snapshot: Arc::new(serde_json::json!({})) };
	assert!(!relay.take("d62c82b23a858876", carried.clone(), Some("d132b0f553e3dcc3")));
	assert!(relay.take("d132b0f553e3dcc3", carried, Some("d132b0f553e3dcc3")));
	assert_eq!(relay.versions(), Versions::from([("d132b0f553e3dcc3".into(), 9)]));
}

/// A row of `node`'s runs at `version`, as a neighbor's batch carries it.
fn ran(node: &str, version: u64) -> Batch {
	let event = crate::host::Event {
		id: 1,
		app: "geo".into(),
		action: "deploy".into(),
		source: crate::host::Source { kind: "panel".into(), run: None, commit: None },
		image: None,
		outcome: "running".into(),
		stage: None,
		detail: None,
		started_at: Timestamp::now().to_string(),
		finished_at: None,
	};
	let rows = vec![crate::runs::Row { version, event }];
	Batch { node: node.into(), above: 0, version, rows }
}

#[test]
fn a_nodes_runs_moving_sends_its_entry_again_with_their_version() {
	let relay = relay("d62c82b23a858876");
	let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
	relay.take(
		"d132b0f553e3dcc3",
		Carried { version, snapshot: Arc::new(serde_json::json!({})) },
		None,
	);
	assert_eq!(relay.state().nodes["d132b0f553e3dcc3"].runs, None);
	let mut changes = relay.subscribe_changes();
	assert!(relay.take_runs(ran("d132b0f553e3dcc3", 7)));
	let change = changes.try_recv().unwrap();
	assert_eq!((change.node.as_str(), change.entry.runs), ("d132b0f553e3dcc3", Some(7)));
	assert_eq!(change.entry.held.map(|held| held.version), Some(version));
	assert_eq!(relay.state().nodes["d132b0f553e3dcc3"].runs, Some(7));
	// What is already held moves nothing, and sends nothing.
	assert!(!relay.take_runs(ran("d132b0f553e3dcc3", 7)));
	assert!(changes.try_recv().is_err());
	// A node neither held nor dialed is not shown for its runs alone.
	assert!(relay.take_runs(ran("e38f403f033d87f7", 3)));
	assert!(changes.try_recv().is_err() && !relay.state().nodes.contains_key("e38f403f033d87f7"));
}

fn round() -> Reading {
	let machine = serde_json::json!({ "info": {}, "sample": { "values": { "cpu.usage": 3.0 } } });
	Reading { events: Ok(vec![]), apps: Ok(vec![]), machine: Ok(machine) }
}

fn own_version(relay: &Relay) -> u64 {
	relay.versions()[relay.node()]
}

#[test]
fn a_version_stands_while_nothing_changes_unless_a_neighbor_older_than_the_beat_is_met() {
	let relay = relay("d62c82b23a858876");
	relay.observe(round());
	let first = own_version(&relay);
	std::thread::sleep(Duration::from_millis(3));
	relay.observe(round());
	assert_eq!(own_version(&relay), first);
	relay.met(1, true);
	relay.met(2, false);
	relay.observe(round());
	let again = own_version(&relay);
	assert!(again > first);
	relay.parted(2);
	std::thread::sleep(Duration::from_millis(3));
	relay.observe(round());
	assert_eq!(own_version(&relay), again);
}

#[test]
fn a_beat_moves_when_a_node_was_heard_and_a_neighbors_beat_of_this_one_is_not_taken() {
	let relay = relay("d62c82b23a858876");
	let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap() - 20_000;
	relay.take(
		"d132b0f553e3dcc3",
		Carried { version, snapshot: Arc::new(serde_json::json!({})) },
		None,
	);
	assert_eq!(relay.state().nodes["d132b0f553e3dcc3"].state, Presence::Late);
	let mut pulses = relay.subscribe_pulses();
	let mut changes = relay.subscribe_changes();
	relay.beaten("d132b0f553e3dcc3", version + 18_000, Some("e38f403f033d87f7"));
	assert!(
		matches!(pulses.try_recv().unwrap(), Pulse::Beat { node, .. } if node == "d132b0f553e3dcc3")
	);
	let change = changes.try_recv().unwrap();
	assert_eq!((change.node.as_str(), change.entry.state), ("d132b0f553e3dcc3", Presence::Live));
	relay.beaten("d62c82b23a858876", version, Some("d132b0f553e3dcc3"));
	assert!(pulses.try_recv().is_err());
	assert_eq!(relay.beats().get("d132b0f553e3dcc3"), Some(&(version + 18_000)));
}

#[test]
fn a_nodes_newest_minute_is_shown_with_its_readings_and_round_trips() {
	let relay = relay("d62c82b23a858876");
	let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
	relay.take(
		"d132b0f553e3dcc3",
		Carried { version, snapshot: Arc::new(serde_json::json!({})) },
		None,
	);
	let mut changes = relay.subscribe_changes();
	let at = crate::history::minute_of(Timestamp::now()) - crate::history::MINUTE;
	let readings =
		Readings::from([("cpu.usage".into(), crate::history::Gauge { mean: 4.0, max: 9.0 })]);
	let minute = Minute {
		at,
		beats: 20,
		down: BTreeMap::new(),
		held: 0,
		round_trip: RoundTrips::from([("d62c82b23a858876".into(), 0.15)]),
		leaving: None,
		readings: readings.clone(),
	};
	let row = crate::history::Row { version: 5, minute };
	let batch = crate::history::Batch {
		node: "d132b0f553e3dcc3".into(),
		above: 0,
		version: 5,
		rows: vec![row],
	};
	assert!(relay.take_minutes(batch));
	let entry = changes.try_recv().unwrap().entry;
	assert_eq!(entry.readings.map(|minutely| minutely.values), Some(readings));
	assert_eq!(entry.round_trip, Some(RoundTrips::from([("d62c82b23a858876".into(), 0.15)])));
	assert!(relay.state().nodes["d132b0f553e3dcc3"].readings.is_some());
}

#[test]
fn a_lease_lends_this_nodes_readings_to_a_page_here_and_to_a_neighbor_holding_one() {
	let relay = relay("d62c82b23a858876");
	let mut pulses = relay.subscribe_pulses();
	let mut lent = relay.subscribe_lent();
	let readings = |pulses: &mut broadcast::Receiver<Pulse>| {
		let pulsed: Vec<Pulse> = std::iter::from_fn(|| pulses.try_recv().ok()).collect();
		pulsed
			.into_iter()
			.any(|pulse| matches!(pulse, Pulse::Readings { node, .. } if node == "d62c82b23a858876"))
	};
	relay.observe(round());
	assert!(!readings(&mut pulses) && lent.try_recv().is_err());
	relay.watch("d62c82b23a858876");
	relay.observe(round());
	assert!(readings(&mut pulses) && lent.try_recv().is_err());
	relay.leased("d132b0f553e3dcc3", Duration::from_secs(600));
	assert!(relay.leased_to("d132b0f553e3dcc3"));
	relay.observe(round());
	assert_eq!(lent.try_recv().unwrap(), serde_json::json!({ "values": { "cpu.usage": 3.0 } }));
}

#[test]
fn a_peer_not_heard_is_shown_waiting_with_nothing_held() {
	let peers = BTreeSet::from(["d132b0f553e3dcc3".to_owned()]);
	let relay =
		Relay::new("d62c82b23a858876".into(), "s3cret".into(), peers, Store::memory().unwrap())
			.unwrap();
	let state = relay.state();
	let waiting =
		Entry { held: None, state: Presence::Waiting, runs: None, readings: None, round_trip: None };
	assert_eq!(state.nodes["d132b0f553e3dcc3"], waiting);
}

#[test]
fn a_state_time_alone_changes_is_sent_again_once() {
	let peers = BTreeSet::from(["d132b0f553e3dcc3".to_owned(), "e38f403f033d87f7".to_owned()]);
	let relay =
		Relay::new("d62c82b23a858876".into(), "s3cret".into(), peers, Store::memory().unwrap())
			.unwrap();
	let now = Timestamp::now();
	let version = u64::try_from(now.as_millisecond()).unwrap();
	relay.take(
		"d132b0f553e3dcc3",
		Carried { version, snapshot: Arc::new(serde_json::json!({})) },
		None,
	);
	let mut changes = relay.subscribe_changes();
	let mut swept = |seconds: i64, uptime: u64| {
		relay.swept(now + jiff::SignedDuration::from_secs(seconds), Duration::from_secs(uptime));
		std::iter::from_fn(|| changes.try_recv().ok())
			.map(|change| (change.node, change.entry.state))
			.collect::<Vec<_>>()
	};
	assert_eq!(swept(5, 5), []);
	assert_eq!(swept(11, 11), [("d132b0f553e3dcc3".to_owned(), Presence::Late)]);
	assert_eq!(swept(12, 12), []);
	// buf, never heard, is given up on once the relay has run a minute.
	let gone = [
		("d132b0f553e3dcc3".to_owned(), Presence::Gone),
		("e38f403f033d87f7".to_owned(), Presence::Gone),
	];
	assert_eq!(swept(61, 61), gone);
}

#[test]
fn a_peer_named_is_shown_waiting_and_one_dropped_unheard_is_not_shown() {
	let peers = BTreeSet::from(["d132b0f553e3dcc3".to_owned(), "e38f403f033d87f7".to_owned()]);
	let relay =
		Relay::new("d62c82b23a858876".into(), "s3cret".into(), peers, Store::memory().unwrap())
			.unwrap();
	let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
	relay.take(
		"d132b0f553e3dcc3",
		Carried { version, snapshot: Arc::new(serde_json::json!({})) },
		None,
	);
	let mut changes = relay.subscribe_changes();
	relay.set_peers(BTreeSet::from(["9d399312d1c3ddc4".to_owned()]));
	let sent: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok()).map(|c| c.node).collect();
	assert_eq!(sent, ["9d399312d1c3ddc4"]);
	let nodes = relay.state().nodes;
	let waiting =
		Entry { held: None, state: Presence::Waiting, runs: None, readings: None, round_trip: None };
	assert_eq!(nodes["9d399312d1c3ddc4"], waiting);
	// tyo, heard, is held until it ages; buf, never heard, is no longer listed.
	assert!(nodes.contains_key("d132b0f553e3dcc3") && !nodes.contains_key("e38f403f033d87f7"));
}
