use super::neighbors::current;
use super::*;

fn relay(node: &str) -> Arc<Relay> {
	Relay::new(node.into(), "s3cret".into(), BTreeSet::new(), Store::memory().unwrap()).unwrap()
}

#[test]
fn one_socket_to_a_neighbor_speaks_until_it_is_hushed() {
	let relay = relay("0000000000000007");
	let (first, second) = (relay.conversation(), relay.conversation());
	assert!(relay.speaks("0000000000000001", first));
	assert!(!relay.speaks("0000000000000001", second));
	assert!(relay.speaks("0000000000000006", second));
	relay.hush("0000000000000001", second);
	assert!(relay.speaks("0000000000000001", first));
	relay.hush("0000000000000001", first);
	assert!(relay.speaks("0000000000000001", second));
}

#[test]
fn a_round_trip_is_in_milliseconds_and_left_out_once_it_is_old() {
	let now = Instant::now();
	let timed = BTreeMap::from([
		("0000000000000001".into(), (Duration::from_micros(151_234), now - Duration::from_secs(1))),
		("0000000000000006".into(), (Duration::from_micros(460), now)),
		("0000000000000004".into(), (Duration::from_millis(90), now - CURRENT)),
	]);
	assert_eq!(
		current(&timed, now),
		RoundTrips::from([("0000000000000001".into(), 0.1512), ("0000000000000006".into(), 0.0005)])
	);
}

#[test]
fn the_kernels_round_trip_stands_over_the_pings_and_the_pings_where_it_has_none() {
	let relay = relay("0000000000000007");
	relay.timed("0000000000000001", Duration::from_millis(137));
	relay.timed("0000000000000008", Duration::from_millis(1234));
	relay.measured("0000000000000001", Duration::from_micros(69_250));
	let round_trips = relay.round_trips();
	assert_eq!(
		round_trips,
		RoundTrips::from([("0000000000000001".into(), 0.0693), ("0000000000000008".into(), 1.234)])
	);
}

#[test]
fn the_latest_round_trip_replaces_the_last() {
	let relay = relay("0000000000000007");
	assert!(relay.round_trips().is_empty());
	relay.timed("0000000000000001", Duration::from_millis(150));
	relay.timed("0000000000000001", Duration::from_millis(149));
	assert_eq!(relay.round_trips(), RoundTrips::from([("0000000000000001".into(), 0.149)]));
}

#[test]
fn a_neighbors_word_on_this_node_is_not_taken() {
	let relay = relay("0000000000000007");
	let carried = Carried { version: 9, snapshot: Arc::new(serde_json::json!({})) };
	assert!(!relay.take("0000000000000007", carried.clone(), Some("0000000000000001")));
	assert!(relay.take("0000000000000001", carried, Some("0000000000000001")));
	assert_eq!(relay.versions(), Versions::from([("0000000000000001".into(), 9)]));
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
	let relay = relay("0000000000000007");
	let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
	relay.take(
		"0000000000000001",
		Carried { version, snapshot: Arc::new(serde_json::json!({})) },
		None,
	);
	assert_eq!(relay.state().nodes["0000000000000001"].runs, None);
	let mut changes = relay.subscribe_changes();
	assert!(relay.take_runs(ran("0000000000000001", 7)));
	let change = changes.try_recv().unwrap();
	assert_eq!((change.node.as_str(), change.entry.runs), ("0000000000000001", Some(7)));
	assert_eq!(change.entry.held.map(|held| held.version), Some(version));
	assert_eq!(relay.state().nodes["0000000000000001"].runs, Some(7));
	// What is already held moves nothing, and sends nothing.
	assert!(!relay.take_runs(ran("0000000000000001", 7)));
	assert!(changes.try_recv().is_err());
	// A node neither held nor dialed is not shown for its runs alone.
	assert!(relay.take_runs(ran("0000000000000006", 3)));
	assert!(changes.try_recv().is_err() && !relay.state().nodes.contains_key("0000000000000006"));
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
	let relay = relay("0000000000000007");
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
	let relay = relay("0000000000000007");
	let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap() - 20_000;
	relay.take(
		"0000000000000001",
		Carried { version, snapshot: Arc::new(serde_json::json!({})) },
		None,
	);
	assert_eq!(relay.state().nodes["0000000000000001"].state, Presence::Late);
	let mut pulses = relay.subscribe_pulses();
	let mut changes = relay.subscribe_changes();
	relay.beaten("0000000000000001", version + 18_000, Some("0000000000000006"));
	assert!(
		matches!(pulses.try_recv().unwrap(), Pulse::Beat { node, .. } if node == "0000000000000001")
	);
	let change = changes.try_recv().unwrap();
	assert_eq!((change.node.as_str(), change.entry.state), ("0000000000000001", Presence::Live));
	relay.beaten("0000000000000007", version, Some("0000000000000001"));
	assert!(pulses.try_recv().is_err());
	assert_eq!(relay.beats().get("0000000000000001"), Some(&(version + 18_000)));
}

#[test]
fn a_nodes_newest_minute_is_shown_with_its_readings_and_round_trips() {
	let relay = relay("0000000000000007");
	let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
	relay.take(
		"0000000000000001",
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
		round_trip: RoundTrips::from([("0000000000000007".into(), 0.15)]),
		leaving: None,
		readings: readings.clone(),
	};
	let row = crate::history::Row { version: 5, minute };
	let batch = crate::history::Batch {
		node: "0000000000000001".into(),
		above: 0,
		version: 5,
		rows: vec![row],
	};
	assert!(relay.take_minutes(batch));
	let entry = changes.try_recv().unwrap().entry;
	assert_eq!(entry.readings.map(|minutely| minutely.values), Some(readings));
	assert_eq!(entry.round_trip, Some(RoundTrips::from([("0000000000000007".into(), 0.15)])));
	assert!(relay.state().nodes["0000000000000001"].readings.is_some());
}

#[test]
fn a_page_watching_is_lent_this_nodes_readings_and_so_is_a_neighbor_holding_a_lease() {
	let relay = relay("0000000000000007");
	let mut pulses = relay.subscribe_pulses();
	let mut lent = relay.subscribe_lent();
	let readings = |pulses: &mut broadcast::Receiver<Pulse>| {
		let pulsed: Vec<Pulse> = std::iter::from_fn(|| pulses.try_recv().ok()).collect();
		pulsed
			.into_iter()
			.any(|pulse| matches!(pulse, Pulse::Readings { node, .. } if node == "0000000000000007"))
	};
	relay.observe(round());
	assert!(!readings(&mut pulses) && lent.try_recv().is_err());
	let watching = relay.watch();
	relay.observe(round());
	assert!(readings(&mut pulses) && lent.try_recv().is_err());
	relay.leased("0000000000000001", Duration::from_secs(600));
	assert!(relay.leased_to("0000000000000001"));
	relay.observe(round());
	assert_eq!(lent.try_recv().unwrap(), serde_json::json!({ "values": { "cpu.usage": 3.0 } }));
	assert!(readings(&mut pulses));
	// The page gone, and the neighbor's lease released: nothing is lent.
	drop(watching);
	relay.leased("0000000000000001", Duration::ZERO);
	assert!(!relay.leased_to("0000000000000001"));
	relay.observe(round());
	assert!(!readings(&mut pulses) && lent.try_recv().is_err());
}

#[test]
fn every_neighbor_is_asked_while_a_page_watches_again_by_half_a_lease_and_released_by_the_last() {
	let relay = relay("0000000000000007");
	let mut asking = relay.subscribe_asking();
	let first = relay.watch();
	assert_eq!(asking.try_recv().unwrap(), LEASE);
	let second = relay.watch();
	assert!(relay.watched() && asking.try_recv().is_err());
	let now = Instant::now();
	relay.renew(now);
	assert!(asking.try_recv().is_err(), "asked within half a lease");
	relay.renew(now + LEASE / 2);
	assert_eq!(asking.try_recv().unwrap(), LEASE);
	drop(first);
	assert!(relay.watched() && asking.try_recv().is_err());
	drop(second);
	assert!(!relay.watched());
	assert_eq!(asking.try_recv().unwrap(), Duration::ZERO);
	relay.renew(now + LEASE * 2);
	assert!(asking.try_recv().is_err(), "nobody watching asks nothing");
}

#[test]
fn a_peer_not_heard_is_shown_waiting_with_nothing_held() {
	let peers = BTreeSet::from(["0000000000000001".to_owned()]);
	let relay =
		Relay::new("0000000000000007".into(), "s3cret".into(), peers, Store::memory().unwrap())
			.unwrap();
	let state = relay.state();
	let waiting =
		Entry { held: None, state: Presence::Waiting, runs: None, readings: None, round_trip: None };
	assert_eq!(state.nodes["0000000000000001"], waiting);
}

#[test]
fn a_state_time_alone_changes_is_sent_again_once() {
	let peers = BTreeSet::from(["0000000000000001".to_owned(), "0000000000000006".to_owned()]);
	let relay =
		Relay::new("0000000000000007".into(), "s3cret".into(), peers, Store::memory().unwrap())
			.unwrap();
	let now = Timestamp::now();
	let version = u64::try_from(now.as_millisecond()).unwrap();
	relay.take(
		"0000000000000001",
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
	assert_eq!(swept(11, 11), [("0000000000000001".to_owned(), Presence::Late)]);
	assert_eq!(swept(12, 12), []);
	// buf, never heard, is given up on once the relay has run a minute.
	let gone = [
		("0000000000000001".to_owned(), Presence::Gone),
		("0000000000000006".to_owned(), Presence::Gone),
	];
	assert_eq!(swept(61, 61), gone);
}

#[test]
fn a_peer_named_is_shown_waiting_and_one_dropped_unheard_is_not_shown() {
	let peers = BTreeSet::from(["0000000000000001".to_owned(), "0000000000000006".to_owned()]);
	let relay =
		Relay::new("0000000000000007".into(), "s3cret".into(), peers, Store::memory().unwrap())
			.unwrap();
	let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
	relay.take(
		"0000000000000001",
		Carried { version, snapshot: Arc::new(serde_json::json!({})) },
		None,
	);
	let mut changes = relay.subscribe_changes();
	relay.set_peers(BTreeSet::from(["0000000000000009".to_owned()]));
	let sent: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok()).map(|c| c.node).collect();
	assert_eq!(sent, ["0000000000000009"]);
	let nodes = relay.state().nodes;
	let waiting =
		Entry { held: None, state: Presence::Waiting, runs: None, readings: None, round_trip: None };
	assert_eq!(nodes["0000000000000009"], waiting);
	// tyo, heard, is held until it ages; buf, never heard, is no longer listed.
	assert!(nodes.contains_key("0000000000000001") && !nodes.contains_key("0000000000000006"));
}
