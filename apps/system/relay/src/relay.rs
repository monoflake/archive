//! One relay's state: what it holds of every node, its own node's snapshot, the runs mirrored, the
//! minutes, and the stream every socket -- a neighbor's or a browser's -- is fed from.

use crate::cluster::{Carried, Cluster, Held, Versions};
use crate::history::{Book, Minute, Readings};
use crate::host::Reading;
use crate::own::{Own, Part, RoundTrips};
use crate::presence::{Entry, Leaving, Minutely, Presence, presence};
use crate::runs::{Batch, Mirror, Shown, Store, StoreError, shared};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::AtomicU64;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch as told};

mod books;
mod deltas;
mod leases;
mod neighbors;
mod tasks;
#[cfg(test)]
mod tests;

pub use leases::Watching;
pub use tasks::{recall, watch, write, written};

/// The version of what the mesh and a browser are sent, read first by either. See the workspace's
/// spec/json.md, "A contract has a version, and it is the first thing read". Version 1 named each
/// node by its name, 2 by its id -- RFC o7eir -- so a relay of each never merges the other's nodes.
pub const VERSION: u32 = 2;

/// How long `/health` waits for the first read of host before it answers healthy regardless: a
/// relay that cannot read its own node still carries every other node.
pub const GRACE: Duration = Duration::from_secs(30);

/// Every node changing every few seconds; a socket further behind than this is caught up whole.
const BACKLOG: usize = 256;

/// A round trip not timed again within three pings is no longer current, and is left out.
const CURRENT: Duration = Duration::from_secs(3 * crate::mesh::PING.as_secs());

/// How long a neighbor lends its fine readings to a page here before it is asked again. See
/// spec/architecture/relay.md, RFC grmtb.
pub const LEASE: Duration = Duration::from_secs(60);

/// A node's newer snapshot, taken here.
#[derive(Debug, Clone)]
pub struct Update {
	pub node: String,
	pub held: Held,
}

/// A node's entry as a browser is sent it: a newer snapshot taken, or a state time alone changed.
#[derive(Debug, Clone)]
pub struct Change {
	pub node: String,
	pub entry: Entry,
}

/// What came of a delta taken: applied, already held, or to be asked for whole.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Applied {
	Taken,
	Held,
	Wanted,
}

/// What a browser is sent between a node's entries: that it was heard, or its fine readings while a
/// page on this relay watches it.
#[derive(Debug, Clone)]
pub enum Pulse {
	Beat { node: String, heard_at: Timestamp },
	Readings { node: String, sample: Value },
}

/// Every node at once: what `/state` answers and a browser's socket opens with.
#[derive(Debug, Serialize)]
pub struct State {
	pub version: u32,
	/// The node whose relay answered.
	pub node: String,
	pub nodes: BTreeMap<String, Entry>,
}

/// Every node's runs of the last 30 days, newest first: what `/runs` answers.
#[derive(Debug, Serialize)]
pub struct Runs {
	pub version: u32,
	/// The node whose relay answered.
	pub node: String,
	pub runs: Vec<Shown>,
}

/// What is held of every node, the state each was last shown in, and the peers dialed, kept under
/// one lock so a browser is sent one node's changes in the order they happened.
#[derive(Debug, Default)]
struct Holding {
	cluster: Cluster,
	shown: BTreeMap<String, Presence>,
	/// The neighbors dialed, shown as waiting until they are heard; changed as control is, see
	/// `registry::follow`.
	peers: BTreeSet<String>,
	/// The version of each node's runs as last shown, so a page reads the runs again when one moves.
	runs: Versions,
	/// Each node's newest minute shown: its readings and round trips between its versions.
	latest: BTreeMap<String, Minute>,
}

impl Holding {
	/// `node`'s entry, `held` as given, in `state`; `timed` is this relay's round trips as they
	/// stand, which its own node's entry shows over its newest minute's.
	fn entry(
		&self,
		node: &str,
		held: Option<Held>,
		state: Presence,
		timed: Option<&RoundTrips>,
	) -> Entry {
		let latest = self.latest.get(node);
		let readings = latest.filter(|minute| !minute.readings.is_empty()).and_then(|minute| {
			let at = Timestamp::from_millisecond(minute.at).ok()?;
			Some(Minutely { at, values: minute.readings.clone() })
		});
		let round_trip = timed
			.cloned()
			.or_else(|| latest.map(|minute| minute.round_trip.clone()))
			.filter(|trips| !trips.is_empty());
		Entry { held, state, runs: self.runs.get(node).copied(), readings, round_trip }
	}

	/// Every node held, and every peer not yet heard, with its state at `now`; `own` is this
	/// relay's node and its round trips as they stand.
	fn entries(
		&self,
		now: Timestamp,
		uptime: Duration,
		own: (&str, &RoundTrips),
	) -> BTreeMap<String, Entry> {
		let held = self.cluster.nodes().iter().map(|(node, held)| (node, Some(held)));
		let unheard = self.peers.iter().filter(|peer| !self.cluster.nodes().contains_key(*peer));
		held
			.chain(unheard.map(|peer| (peer, None)))
			.map(|(node, held)| {
				let state = presence(held, now, uptime);
				let timed = (node == own.0).then_some(own.1);
				(node.clone(), self.entry(node, held.cloned(), state, timed))
			})
			.collect()
	}
}

pub struct Relay {
	node: String,
	secret: String,
	holding: Mutex<Holding>,
	own: Mutex<Own>,
	updates: broadcast::Sender<Update>,
	changes: broadcast::Sender<Change>,
	pulses: broadcast::Sender<Pulse>,
	/// This node's own beats, its clock in milliseconds, for the mesh to send each neighbor.
	beats: broadcast::Sender<u64>,
	/// The lease asked of every neighbor that lends: `LEASE` while a page here watches, and none
	/// once the last stops.
	asking: broadcast::Sender<Duration>,
	/// The pages here watching every node.
	watchers: Mutex<leases::Watchers>,
	/// This node's fine readings each round while a neighbor holds a lease on them.
	lent: broadcast::Sender<Value>,
	/// Which neighbors hold a lease on this node's fine readings, and until when.
	leases: Mutex<BTreeMap<String, Instant>>,
	/// Each open conversation with a neighbor, and whether that neighbor hears by beats.
	met: Mutex<BTreeMap<u64, bool>>,
	/// Whether this relay says it reads compressed frames, and sends them to a neighbor that says so.
	compressing: std::sync::atomic::AtomicBool,
	/// Whether this relay says it syncs by digest, and syncs with a neighbor that says so.
	syncing: std::sync::atomic::AtomicBool,
	runs: Mirror,
	/// This node's own rows as each round finds them new or changed, for the mesh to push.
	batches: broadcast::Sender<Batch>,
	history: Book,
	/// This node's own minutes as each ends, for the mesh to push.
	minutes: broadcast::Sender<crate::history::Batch>,
	started: Instant,
	/// Which socket to each neighbor sends unasked; see `mesh::Speaking`.
	speakers: Mutex<BTreeMap<String, u64>>,
	conversations: AtomicU64,
	/// Each neighbor's latest round trip by its pings, and when it was timed.
	timed: Mutex<BTreeMap<String, (Duration, Instant)>>,
	/// Each neighbor's latest round trip by the kernel, and when it was read; over `timed` while
	/// current. See `tcp`.
	measured: Mutex<BTreeMap<String, (Duration, Instant)>>,
	/// The newest of this node's own versions sent to each neighbor, for a relay leaving to wait on.
	told: told::Sender<Versions>,
	/// The nodes control lists as retired, whose relays are refused; see `registry::follow`.
	retired: Mutex<BTreeSet<String>>,
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
	mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl Relay {
	/// A relay over its file of runs and minutes, read before anything else is.
	pub fn new(
		node: String,
		secret: String,
		peers: BTreeSet<String>,
		store: Store,
	) -> Result<Arc<Self>, StoreError> {
		let shown = peers.iter().map(|peer| (peer.clone(), Presence::Waiting)).collect();
		let store = shared(store);
		let runs = Mirror::open(node.clone(), store.clone())?;
		let holding = Holding {
			cluster: Cluster::default(),
			shown,
			peers,
			runs: runs.versions(),
			latest: BTreeMap::new(),
		};
		Ok(Arc::new(Self {
			runs,
			history: Book::open(node.clone(), store)?,
			minutes: broadcast::channel(BACKLOG).0,
			node,
			secret,
			holding: Mutex::new(holding),
			own: Mutex::default(),
			updates: broadcast::channel(BACKLOG).0,
			changes: broadcast::channel(BACKLOG).0,
			pulses: broadcast::channel(BACKLOG).0,
			beats: broadcast::channel(BACKLOG).0,
			asking: broadcast::channel(BACKLOG).0,
			lent: broadcast::channel(BACKLOG).0,
			watchers: Mutex::default(),
			leases: Mutex::default(),
			met: Mutex::default(),
			compressing: std::sync::atomic::AtomicBool::new(true),
			syncing: std::sync::atomic::AtomicBool::new(true),
			batches: broadcast::channel(BACKLOG).0,
			started: Instant::now(),
			speakers: Mutex::default(),
			conversations: AtomicU64::new(0),
			timed: Mutex::default(),
			measured: Mutex::default(),
			told: told::Sender::new(Versions::new()),
			retired: Mutex::default(),
		}))
	}

	pub fn node(&self) -> &str {
		&self.node
	}

	pub fn secret(&self) -> &str {
		&self.secret
	}

	pub fn subscribe(&self) -> broadcast::Receiver<Update> {
		self.updates.subscribe()
	}

	pub fn subscribe_changes(&self) -> broadcast::Receiver<Change> {
		self.changes.subscribe()
	}

	pub fn subscribe_pulses(&self) -> broadcast::Receiver<Pulse> {
		self.pulses.subscribe()
	}

	pub fn subscribe_beats(&self) -> broadcast::Receiver<u64> {
		self.beats.subscribe()
	}

	pub fn subscribe_asking(&self) -> broadcast::Receiver<Duration> {
		self.asking.subscribe()
	}

	pub fn subscribe_lent(&self) -> broadcast::Receiver<Value> {
		self.lent.subscribe()
	}

	/// Takes a snapshot of `node` when it is newer than what is held, and passes it on; whether it
	/// did. `via` is the neighbor it came from, whose word on this node is never taken: this relay is
	/// its one source.
	pub fn take(&self, node: &str, carried: Carried, via: Option<&str>) -> bool {
		if via.is_some() && node == self.node {
			return false;
		}
		let now = Timestamp::now();
		let mut holding = lock(&self.holding);
		if !holding.cluster.merge(node, carried, now) {
			return false;
		}
		// Sent under the lock, so every socket sees one node's versions in the order taken.
		let held = holding.cluster.nodes()[node].clone();
		let state = presence(Some(&held), now, self.started.elapsed());
		holding.shown.insert(node.to_owned(), state);
		let entry = holding.entry(node, Some(held.clone()), state, None);
		let _ = self.updates.send(Update { node: node.to_owned(), held });
		let _ = self.changes.send(Change { node: node.to_owned(), entry });
		true
	}

	/// One round of reading host, now.
	pub fn observe(&self, reading: Reading) {
		self.observe_at(reading, None, Timestamp::now());
	}

	/// One round of reading host at `now`, with the meter's minute where one just ended: its
	/// events into the runs, the round and the round trips into the minute, a new version of this
	/// node's snapshot where what it says changed, a beat, and the fine readings to whoever holds a
	/// lease on them.
	pub fn observe_at(&self, reading: Reading, minute: Option<(i64, Readings)>, now: Timestamp) {
		if let Ok(events) = &reading.events {
			self.events_read(events, now);
		}
		let round_trips = self.round_trips();
		if let Some(batch) = self.history.round(&reading, &round_trips, minute.clone(), now) {
			let _ = self.minutes.send(batch);
			self.minute_moved(&self.node);
		}
		let every_round = self.every_round();
		let (changed, sample) = {
			let mut own = lock(&self.own);
			let before: BTreeSet<Part> = own.stale().clone();
			let (changed, failed) = own.observe(reading, minute.as_ref(), every_round, now);
			for (part, error) in &failed {
				if !before.contains(part) {
					eprintln!("relay: reading host's {part}: {error}");
				}
			}
			for part in before.difference(own.stale()) {
				eprintln!("relay: reading host's {part} again");
			}
			(changed, own.sample().cloned())
		};
		if let Some(changed) = changed {
			self.take_own(changed);
		}
		let at = u64::try_from(now.as_millisecond()).unwrap_or(0);
		self.beaten(&self.node, at, None);
		let _ = self.beats.send(at);
		self.renew(Instant::now());
		if let Some(sample) = sample {
			self.lend(sample);
		}
	}

	/// `node` heard at `at`, its origin's clock, from `via` or from this relay itself: its
	/// `heard_at` moved where the beat is newer, a browser told, and its state sent again where the
	/// beat moved it. A neighbor's word on this node is never taken.
	pub fn beaten(&self, node: &str, at: u64, via: Option<&str>) {
		if via.is_some() && node == self.node {
			return;
		}
		let now = Timestamp::now();
		let timed = (node == self.node).then(|| self.round_trips());
		let mut holding = lock(&self.holding);
		let Some(heard_at) = holding.cluster.beat(node, at, now) else {
			return;
		};
		let _ = self.pulses.send(Pulse::Beat { node: node.to_owned(), heard_at });
		let held = holding.cluster.nodes().get(node).cloned();
		let state = presence(held.as_ref(), now, self.started.elapsed());
		if holding.shown.get(node) != Some(&state) {
			holding.shown.insert(node.to_owned(), state);
			let entry = holding.entry(node, held, state, timed.as_ref());
			let _ = self.changes.send(Change { node: node.to_owned(), entry });
		}
	}

	/// Each node's newest beat held, for the comparison to carry.
	pub fn beats(&self) -> Versions {
		lock(&self.holding).cluster.beats()
	}

	/// `node`'s newest minute moved here: its entry sent again with its readings and round trips.
	fn minute_moved(&self, node: &str) {
		let Some(latest) = self.history.latest(node) else {
			return;
		};
		let now = Timestamp::now();
		let timed = (node == self.node).then(|| self.round_trips());
		let mut holding = lock(&self.holding);
		if holding.latest.get(node).is_some_and(|shown| shown.at >= latest.at) {
			return;
		}
		holding.latest.insert(node.to_owned(), latest);
		let held = holding.cluster.nodes().get(node).cloned();
		if held.is_none() && !holding.peers.contains(node) {
			return;
		}
		let state = presence(held.as_ref(), now, self.started.elapsed());
		holding.shown.insert(node.to_owned(), state);
		let entry = holding.entry(node, held, state, timed.as_ref());
		let _ = self.changes.send(Change { node: node.to_owned(), entry });
	}

	/// The last snapshot of this node, saying it is leaving, taken and so pushed to the neighbors;
	/// its version.
	pub fn leave(&self, leaving: Leaving) -> u64 {
		let now = Timestamp::now();
		let _ = self.minutes.send(self.history.leave(leaving.reason, now));
		let last = lock(&self.own).leave(leaving, now);
		let version = last.0;
		self.take_own(last);
		version
	}

	fn take_own(&self, (version, snapshot): (u64, crate::own::Snapshot)) {
		let snapshot = serde_json::to_value(snapshot).expect("a snapshot is a tree of strings");
		self.take(&self.node, Carried { version, snapshot: Arc::new(snapshot) }, None);
	}

	/// This node's own events, as a round or the reading back reads them, into the runs; what is
	/// new or changed is pushed to the neighbors.
	fn events_read(&self, events: &[crate::host::Event], now: Timestamp) {
		if let Some(batch) = self.runs.observe(events, now) {
			let _ = self.batches.send(batch);
			self.runs_moved(&self.node);
		}
	}

	/// `node`'s runs moved here: its entry sent again carrying their version, so a page reading them
	/// reads again. An older version than shown, from a second socket, sends nothing.
	fn runs_moved(&self, node: &str) {
		let Some(version) = self.runs.versions().get(node).copied() else {
			return;
		};
		let now = Timestamp::now();
		let mut holding = lock(&self.holding);
		if holding.runs.get(node).is_some_and(|shown| *shown >= version) {
			return;
		}
		holding.runs.insert(node.to_owned(), version);
		let held = holding.cluster.nodes().get(node).cloned();
		if held.is_none() && !holding.peers.contains(node) {
			return;
		}
		let state = presence(held.as_ref(), now, self.started.elapsed());
		holding.shown.insert(node.to_owned(), state);
		let entry = holding.entry(node, held, state, None);
		let _ = self.changes.send(Change { node: node.to_owned(), entry });
	}

	pub fn state(&self) -> State {
		let timed = self.round_trips();
		let own = (self.node.as_str(), &timed);
		let nodes = lock(&self.holding).entries(Timestamp::now(), self.started.elapsed(), own);
		State { version: VERSION, node: self.node.clone(), nodes }
	}
}
