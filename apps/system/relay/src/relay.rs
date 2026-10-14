//! One relay's state: what it holds of every node, its own node's snapshot, the runs mirrored, the
//! minutes, and the stream every socket -- a neighbor's or a browser's -- is fed from.

use crate::cluster::{Carried, Cluster, Held, Versions};
use crate::delta::Changes;
use crate::history::answer::{Asked, History};
use crate::history::{Book, DAY, Minute, Readings, minute_of};
use crate::host::{PAGE, Reader, Reading};
use crate::own::{Own, Part, RoundTrips};
use crate::presence::{Entry, Leaving, Minutely, Presence, presence};
use crate::runs::{Batch, Mirror, Shown, Store, StoreError, shared};
use jiff::Timestamp;
use serde::Serialize;
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, watch as told};

/// The version of what the mesh and a browser are sent, read first by either. See the workspace's
/// spec/json.md, "A contract has a version, and it is the first thing read".
pub const VERSION: u32 = 1;

/// How long `/health` waits for the first read of host before it answers healthy regardless: a
/// relay that cannot read its own node still carries every other node.
pub const GRACE: Duration = Duration::from_secs(30);

/// Every node changing every few seconds; a socket further behind than this is caught up whole.
const BACKLOG: usize = 256;

/// A round trip not timed again within three pings is no longer current, and is left out.
const CURRENT: Duration = Duration::from_secs(3 * crate::mesh::PING.as_secs());

/// How long a page's ask for a node's fine readings holds before it is asked again. See
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
	/// The neighbors dialed, shown as waiting until they are heard; changed as the registry is, see
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
	/// The nodes a page here asked the fine readings of, for the mesh to ask each origin.
	asking: broadcast::Sender<String>,
	/// This node's fine readings each round while a neighbor holds a lease on them.
	lent: broadcast::Sender<Value>,
	/// Who holds a lease on this node's fine readings, and until when: a neighbor by its name, a
	/// page on this relay by this node's.
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
	/// The nodes the registry lists as retired, whose relays are refused; see `registry::follow`.
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

	pub fn subscribe_asking(&self) -> broadcast::Receiver<String> {
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

	/// Each node held, with its version, the digest of it, and its newest beat: what a sync says.
	pub fn lines(&self) -> BTreeMap<String, (u64, u64, u64)> {
		let holding = lock(&self.holding);
		let (cluster, beats) = (&holding.cluster, holding.cluster.beats());
		let digest = |node: &str| cluster.digest(node).unwrap_or_default();
		let beat = |node: &str| beats.get(node).copied().unwrap_or_default();
		let nodes = cluster.nodes().iter();
		nodes.map(|(node, held)| (node.clone(), (held.version, digest(node), beat(node)))).collect()
	}

	/// `node` as held, whole.
	pub fn whole(&self, node: &str) -> Option<Carried> {
		let holding = lock(&self.holding);
		let held = holding.cluster.nodes().get(node)?;
		Some(Carried { version: held.version, snapshot: held.snapshot.clone() })
	}

	/// What takes `node` from `base` to the version held: that version, its digest, and the
	/// changes; none where `base` is no longer kept.
	pub fn delta_from(&self, node: &str, base: u64) -> Option<(u64, u64, Changes)> {
		let holding = lock(&self.holding);
		let (cluster, held) = (&holding.cluster, holding.cluster.nodes().get(node)?);
		let from = cluster.at(node, base)?;
		Some((held.version, cluster.digest(node)?, crate::delta::diff(&from, &held.snapshot)))
	}

	/// A neighbor's delta of `node`, from `base` to `version`, taken where this relay holds `base`
	/// and the result reads `digest`. A neighbor's word on this node is never taken.
	pub fn take_delta(
		&self,
		node: &str,
		(base, version): (u64, u64),
		digest: &str,
		changes: &Changes,
		via: &str,
	) -> Applied {
		if node == self.node {
			return Applied::Held;
		}
		let held = {
			let holding = lock(&self.holding);
			holding.cluster.nodes().get(node).map(|held| (held.version, held.snapshot.clone()))
		};
		let snapshot = match held {
			Some((held, _)) if held >= version => return Applied::Held,
			Some((held, snapshot)) if held == base => snapshot,
			_ => return Applied::Wanted,
		};
		let Some(next) = crate::delta::apply(&snapshot, changes) else {
			return Applied::Wanted;
		};
		if crate::delta::hex(crate::delta::digest(&next)) != digest {
			return Applied::Wanted;
		}
		self.take(node, Carried { version, snapshot: Arc::new(next) }, Some(via));
		Applied::Taken
	}

	/// Whether a neighbor older than the beat is met, which hears a node by its versions alone: so
	/// long as one is, this node's own is given a new version every round.
	fn every_round(&self) -> bool {
		lock(&self.met).values().any(|beats| !beats)
	}

	/// A conversation with a neighbor introduced, and whether that neighbor hears by beats.
	pub fn met(&self, conversation: u64, beats: bool) {
		lock(&self.met).insert(conversation, beats);
	}

	/// A conversation ended.
	pub fn parted(&self, conversation: u64) {
		lock(&self.met).remove(&conversation);
	}

	/// Whether this relay reads and sends compressed frames; see `compression`.
	pub fn compresses(&self) -> bool {
		self.compressing.load(Ordering::Relaxed)
	}

	/// Turns compressed frames off from the next conversation on: a relay that stands in for one
	/// older than them, as a test does.
	pub fn set_compression(&self, on: bool) {
		self.compressing.store(on, Ordering::Relaxed);
	}

	/// Whether this relay syncs by digest; see `mesh::Message::Sync`.
	pub fn syncs(&self) -> bool {
		self.syncing.load(Ordering::Relaxed)
	}

	/// Turns the sync off from the next conversation on, as `set_compression` turns compression.
	pub fn set_sync(&self, on: bool) {
		self.syncing.store(on, Ordering::Relaxed);
	}

	/// A page here asks for `node`'s fine readings: this node's own lent at once, any other's asked
	/// of its origin over the mesh, for `LEASE`.
	pub fn watch(&self, node: &str) {
		if node == self.node {
			lock(&self.leases).insert(self.node.clone(), Instant::now() + LEASE);
		} else {
			let _ = self.asking.send(node.to_owned());
		}
	}

	/// `peer` holds a lease on this node's fine readings for `lease`, at most `LEASE`.
	pub fn leased(&self, peer: &str, lease: Duration) {
		lock(&self.leases).insert(peer.to_owned(), Instant::now() + lease.min(LEASE));
	}

	/// Whether `peer`, or a page here by this node's name, holds a lease still.
	pub fn leased_to(&self, peer: &str) -> bool {
		lock(&self.leases).get(peer).is_some_and(|until| *until > Instant::now())
	}

	/// This node's fine readings of the round, to a page here and to every neighbor with a lease.
	fn lend(&self, sample: Value) {
		let neighbors = {
			let now = Instant::now();
			let mut leases = lock(&self.leases);
			leases.retain(|_, until| *until > now);
			leases.keys().any(|holder| *holder != self.node)
		};
		if self.leased_to(&self.node) {
			let node = self.node.clone();
			let _ = self.pulses.send(Pulse::Readings { node, sample: sample.clone() });
		}
		if neighbors {
			let _ = self.lent.send(sample);
		}
	}

	/// A node's fine readings, from its origin, for a page here.
	pub fn heard_readings(&self, node: &str, sample: Value) {
		let _ = self.pulses.send(Pulse::Readings { node: node.to_owned(), sample });
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

	pub fn versions(&self) -> Versions {
		lock(&self.holding).cluster.versions()
	}

	/// Takes a neighbor's batch of runs; whether it did. Never this node's own, as `take`.
	pub fn take_runs(&self, batch: Batch) -> bool {
		let node = batch.node.clone();
		let taken = self.runs.merge(batch);
		if taken {
			self.runs_moved(&node);
		}
		taken
	}

	pub fn run_versions(&self) -> Versions {
		self.runs.versions()
	}

	pub async fn lacking_runs(
		&self,
		theirs: &Versions,
		peer: &str,
	) -> Result<Vec<Batch>, StoreError> {
		self.runs.lacking(theirs, peer).await
	}

	pub fn subscribe_runs(&self) -> broadcast::Receiver<Batch> {
		self.batches.subscribe()
	}

	/// The window written, and what is past keeping dropped. See `runs::Mirror::write`.
	pub async fn write_runs(&self) -> Result<(), StoreError> {
		self.runs.write(Timestamp::now()).await
	}

	/// Takes a neighbor's batch of minutes; whether it did. Never this node's own, as `take`.
	pub fn take_minutes(&self, batch: crate::history::Batch) -> bool {
		let node = batch.node.clone();
		let taken = self.history.merge(batch);
		if taken {
			self.minute_moved(&node);
		}
		taken
	}

	pub fn minute_versions(&self) -> Versions {
		self.history.versions()
	}

	pub async fn lacking_minutes(
		&self,
		theirs: &Versions,
		peer: &str,
	) -> Result<Vec<crate::history::Batch>, StoreError> {
		self.history.lacking(theirs, peer).await
	}

	pub fn subscribe_minutes(&self) -> broadcast::Receiver<crate::history::Batch> {
		self.minutes.subscribe()
	}

	/// The minutes' window written, and each tier past keeping folded and dropped. See
	/// `history::Book::write`.
	pub async fn write_history(&self) -> Result<(), StoreError> {
		self.history.write(Timestamp::now()).await
	}

	/// Every node over the span `asked`, from the file; the runs' rows from a day before it, so a
	/// run begun before it is not counted as begun in it.
	pub async fn history(&self, asked: Asked) -> Result<History, StoreError> {
		let now = Timestamp::now();
		let (from, _) = asked.bounds(now.as_millisecond());
		let runs = self.runs.since(from - DAY, now).await?;
		self.history.history(self.node.clone(), asked, runs, now).await
	}

	/// Every node's rows, from `since` where it is given and with every one still running, and
	/// `lean` without what no reader of a span or a count reads: the image, the stage, the commit,
	/// and why a row failed where it did not.
	pub async fn runs(&self, since: Option<i64>, lean: bool) -> Result<Runs, StoreError> {
		let mut runs = self.runs.listed(since, Timestamp::now()).await?;
		if lean {
			for shown in &mut runs {
				let event = &mut shown.event;
				event.image = None;
				event.stage = None;
				event.source.commit = None;
				if event.outcome != "failed" {
					event.detail = None;
				}
			}
		}
		Ok(Runs { version: VERSION, node: self.node.clone(), runs })
	}

	pub fn lacking(&self, theirs: &Versions, peer: &str) -> Vec<(String, Carried)> {
		lock(&self.holding).cluster.lacking(theirs, peer)
	}

	pub fn state(&self) -> State {
		let timed = self.round_trips();
		let own = (self.node.as_str(), &timed);
		let nodes = lock(&self.holding).entries(Timestamp::now(), self.started.elapsed(), own);
		State { version: VERSION, node: self.node.clone(), nodes }
	}

	/// The nodes held as retired from now on, every one named.
	pub fn set_retired(&self, retired: BTreeSet<String>) {
		*lock(&self.retired) = retired;
	}

	/// Whether `node` is held as retired, so its relay is not spoken with.
	pub fn is_retired(&self, node: &str) -> bool {
		lock(&self.retired).contains(node)
	}

	/// The neighbors dialed from now on. A new one is shown waiting at once; one no longer dialed
	/// stops being shown waiting, and what is held of it ages as any silent node's does.
	pub fn set_peers(&self, peers: BTreeSet<String>) {
		{
			let mut holding = lock(&self.holding);
			let Holding { cluster, shown, peers: held, .. } = &mut *holding;
			for gone in held.difference(&peers) {
				if !cluster.nodes().contains_key(gone) {
					shown.remove(gone);
				}
			}
			*held = peers;
		}
		self.sweep();
	}

	/// Every node whose state time alone has changed since it was last shown, sent again.
	pub fn sweep(&self) {
		self.swept(Timestamp::now(), self.started.elapsed());
	}

	fn swept(&self, now: Timestamp, uptime: Duration) {
		let timed = self.round_trips();
		let mut holding = lock(&self.holding);
		for (node, entry) in holding.entries(now, uptime, (self.node.as_str(), &timed)) {
			if holding.shown.get(&node) != Some(&entry.state) {
				holding.shown.insert(node.clone(), entry.state);
				let _ = self.changes.send(Change { node, entry });
			}
		}
	}

	/// A number for a socket to a neighbor, its own among them.
	pub fn conversation(&self) -> u64 {
		self.conversations.fetch_add(1, Ordering::Relaxed)
	}

	/// Whether `conversation` is the socket to `peer` that sends unasked: the first to ask is, until
	/// it is hushed.
	pub fn speaks(&self, peer: &str, conversation: u64) -> bool {
		*lock(&self.speakers).entry(peer.to_owned()).or_insert(conversation) == conversation
	}

	pub fn hush(&self, peer: &str, conversation: u64) {
		let mut speakers = lock(&self.speakers);
		if speakers.get(peer) == Some(&conversation) {
			speakers.remove(peer);
		}
	}

	/// The neighbors a socket is open to and speaking.
	pub fn speaking(&self) -> BTreeSet<String> {
		lock(&self.speakers).keys().cloned().collect()
	}

	/// This node's own `version` sent to `peer`.
	pub fn told(&self, peer: &str, version: u64) {
		self.told.send_modify(|told| {
			let sent = told.entry(peer.to_owned()).or_default();
			*sent = (*sent).max(version);
		});
	}

	/// Once every one of `neighbors` has been sent this node's own `version` or a newer one.
	pub async fn told_of(&self, neighbors: &BTreeSet<String>, version: u64) {
		let mut told = self.told.subscribe();
		let every =
			|told: &Versions| neighbors.iter().all(|peer| told.get(peer).is_some_and(|v| *v >= version));
		// The sender lives as long as the relay, which outlives this wait.
		let _ = told.wait_for(every).await;
	}

	/// A round trip to `peer` just timed, on either socket to it; the latest replaces the last.
	pub fn timed(&self, peer: &str, round_trip: Duration) {
		lock(&self.timed).insert(peer.to_owned(), (round_trip, Instant::now()));
	}

	/// The kernel's round trip to `peer` just read, on the socket dialed to it.
	pub fn measured(&self, peer: &str, round_trip: Duration) {
		lock(&self.measured).insert(peer.to_owned(), (round_trip, Instant::now()));
	}

	/// Each neighbor's latest round trip, the kernel's over the pings', a neighbor not timed lately
	/// left out.
	pub fn round_trips(&self) -> RoundTrips {
		let now = Instant::now();
		let mut round_trips = current(&lock(&self.timed), now);
		round_trips.extend(current(&lock(&self.measured), now));
		round_trips
	}

	/// Once host has been read, or the grace is over.
	pub fn healthy(&self) -> bool {
		lock(&self.own).read() || self.started.elapsed() >= GRACE
	}
}

/// The round trips timed within `CURRENT` of `now`, in seconds to a ten-thousandth -- a tenth of a
/// millisecond, since a neighbor in the same place answers in under one; workspace spec/json.md,
/// "Keys are stable; values are cheap", has durations in seconds.
fn current(timed: &BTreeMap<String, (Duration, Instant)>, now: Instant) -> RoundTrips {
	timed
		.iter()
		.filter(|(_, (_, at))| now.saturating_duration_since(*at) < CURRENT)
		.map(|(peer, (round_trip, _))| {
			(peer.clone(), (round_trip.as_secs_f64() * 10_000.0).round() / 10_000.0)
		})
		.collect()
}

/// Reads host every `every`, for as long as the relay runs, and the meter's minute once each
/// minute ends.
pub async fn watch(relay: Arc<Relay>, source: Arc<dyn crate::host::Reader>, every: Duration) {
	let mut ticks = tokio::time::interval(every);
	ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	let mut counting = minute_of(Timestamp::now());
	loop {
		ticks.tick().await;
		let reading = source.read().await;
		let now = Timestamp::now();
		let mut minute = None;
		if minute_of(now) != counting {
			let ended = std::mem::replace(&mut counting, minute_of(now));
			minute = source.minute(ended).await.map(|readings| (ended, readings));
		}
		relay.observe_at(reading, minute, now);
	}
}

/// Reads this node's host back to the oldest row kept, a page at a time, into the runs: once, as
/// the relay starts, beside its rounds. A page host does not answer is asked again after a write's
/// wait. See spec/architecture/relay.md, "The runs, mirrored on every relay's disk".
pub async fn recall(relay: Arc<Relay>, source: Arc<dyn Reader>) {
	let mut before = None;
	let mut read = 0;
	loop {
		let page = match source.page(before).await {
			Ok(page) => page,
			Err(error) => {
				eprintln!("relay: reading host's earlier events: {error}");
				tokio::time::sleep(crate::runs::WRITE).await;
				continue;
			}
		};
		let now = Timestamp::now();
		relay.events_read(&page, now);
		read += page.len();
		let cutoff = now - crate::runs::KEPT;
		let past = page
			.iter()
			.any(|event| event.started_at.parse::<Timestamp>().is_ok_and(|started| started < cutoff));
		match page.iter().map(|event| event.id).min() {
			Some(oldest) if page.len() >= PAGE && read < crate::runs::MOST && !past => {
				before = Some(oldest);
			}
			_ => return,
		}
	}
}

/// Writes the runs and the minutes every `every`, for as long as the relay runs. A refusal is
/// logged and loses nothing: the rows stay for the next.
pub async fn write(relay: Arc<Relay>, every: Duration) {
	let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + every, every);
	ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
	loop {
		ticks.tick().await;
		written(&relay).await;
	}
}

/// The runs and the minutes written, each refusal logged.
pub async fn written(relay: &Relay) {
	if let Err(error) = relay.write_runs().await {
		eprintln!("relay: writing the runs: {error}");
	}
	if let Err(error) = relay.write_history().await {
		eprintln!("relay: writing the minutes: {error}");
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	fn relay(node: &str) -> Arc<Relay> {
		Relay::new(node.into(), "s3cret".into(), BTreeSet::new(), Store::memory().unwrap()).unwrap()
	}

	#[test]
	fn one_socket_to_a_neighbor_speaks_until_it_is_hushed() {
		let relay = relay("rdu");
		let (first, second) = (relay.conversation(), relay.conversation());
		assert!(relay.speaks("tyo", first));
		assert!(!relay.speaks("tyo", second));
		assert!(relay.speaks("buf", second));
		relay.hush("tyo", second);
		assert!(relay.speaks("tyo", first));
		relay.hush("tyo", first);
		assert!(relay.speaks("tyo", second));
	}

	#[test]
	fn a_round_trip_is_in_milliseconds_and_left_out_once_it_is_old() {
		let now = Instant::now();
		let timed = BTreeMap::from([
			("tyo".into(), (Duration::from_micros(151_234), now - Duration::from_secs(1))),
			("buf".into(), (Duration::from_micros(460), now)),
			("gvx".into(), (Duration::from_millis(90), now - CURRENT)),
		]);
		assert_eq!(
			current(&timed, now),
			RoundTrips::from([("tyo".into(), 0.1512), ("buf".into(), 0.0005)])
		);
	}

	#[test]
	fn the_kernels_round_trip_stands_over_the_pings_and_the_pings_where_it_has_none() {
		let relay = relay("rdu");
		relay.timed("tyo", Duration::from_millis(137));
		relay.timed("sha", Duration::from_millis(1234));
		relay.measured("tyo", Duration::from_micros(69_250));
		let round_trips = relay.round_trips();
		assert_eq!(round_trips, RoundTrips::from([("tyo".into(), 0.0693), ("sha".into(), 1.234)]));
	}

	#[test]
	fn the_latest_round_trip_replaces_the_last() {
		let relay = relay("rdu");
		assert!(relay.round_trips().is_empty());
		relay.timed("tyo", Duration::from_millis(150));
		relay.timed("tyo", Duration::from_millis(149));
		assert_eq!(relay.round_trips(), RoundTrips::from([("tyo".into(), 0.149)]));
	}

	#[test]
	fn a_neighbors_word_on_this_node_is_not_taken() {
		let relay = relay("rdu");
		let carried = Carried { version: 9, snapshot: Arc::new(serde_json::json!({})) };
		assert!(!relay.take("rdu", carried.clone(), Some("tyo")));
		assert!(relay.take("tyo", carried, Some("tyo")));
		assert_eq!(relay.versions(), Versions::from([("tyo".into(), 9)]));
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
		let relay = relay("rdu");
		let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
		relay.take("tyo", Carried { version, snapshot: Arc::new(serde_json::json!({})) }, None);
		assert_eq!(relay.state().nodes["tyo"].runs, None);
		let mut changes = relay.subscribe_changes();
		assert!(relay.take_runs(ran("tyo", 7)));
		let change = changes.try_recv().unwrap();
		assert_eq!((change.node.as_str(), change.entry.runs), ("tyo", Some(7)));
		assert_eq!(change.entry.held.map(|held| held.version), Some(version));
		assert_eq!(relay.state().nodes["tyo"].runs, Some(7));
		// What is already held moves nothing, and sends nothing.
		assert!(!relay.take_runs(ran("tyo", 7)));
		assert!(changes.try_recv().is_err());
		// A node neither held nor dialed is not shown for its runs alone.
		assert!(relay.take_runs(ran("buf", 3)));
		assert!(changes.try_recv().is_err() && !relay.state().nodes.contains_key("buf"));
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
		let relay = relay("rdu");
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
		let relay = relay("rdu");
		let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap() - 20_000;
		relay.take("tyo", Carried { version, snapshot: Arc::new(serde_json::json!({})) }, None);
		assert_eq!(relay.state().nodes["tyo"].state, Presence::Late);
		let mut pulses = relay.subscribe_pulses();
		let mut changes = relay.subscribe_changes();
		relay.beaten("tyo", version + 18_000, Some("buf"));
		assert!(matches!(pulses.try_recv().unwrap(), Pulse::Beat { node, .. } if node == "tyo"));
		let change = changes.try_recv().unwrap();
		assert_eq!((change.node.as_str(), change.entry.state), ("tyo", Presence::Live));
		relay.beaten("rdu", version, Some("tyo"));
		assert!(pulses.try_recv().is_err());
		assert_eq!(relay.beats().get("tyo"), Some(&(version + 18_000)));
	}

	#[test]
	fn a_nodes_newest_minute_is_shown_with_its_readings_and_round_trips() {
		let relay = relay("rdu");
		let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
		relay.take("tyo", Carried { version, snapshot: Arc::new(serde_json::json!({})) }, None);
		let mut changes = relay.subscribe_changes();
		let at = crate::history::minute_of(Timestamp::now()) - crate::history::MINUTE;
		let readings =
			Readings::from([("cpu.usage".into(), crate::history::Gauge { mean: 4.0, max: 9.0 })]);
		let minute = Minute {
			at,
			beats: 20,
			down: BTreeMap::new(),
			held: 0,
			round_trip: RoundTrips::from([("rdu".into(), 0.15)]),
			leaving: None,
			readings: readings.clone(),
		};
		let row = crate::history::Row { version: 5, minute };
		let batch = crate::history::Batch { node: "tyo".into(), above: 0, version: 5, rows: vec![row] };
		assert!(relay.take_minutes(batch));
		let entry = changes.try_recv().unwrap().entry;
		assert_eq!(entry.readings.map(|minutely| minutely.values), Some(readings));
		assert_eq!(entry.round_trip, Some(RoundTrips::from([("rdu".into(), 0.15)])));
		assert!(relay.state().nodes["tyo"].readings.is_some());
	}

	#[test]
	fn a_lease_lends_this_nodes_readings_to_a_page_here_and_to_a_neighbor_holding_one() {
		let relay = relay("rdu");
		let mut pulses = relay.subscribe_pulses();
		let mut lent = relay.subscribe_lent();
		let readings = |pulses: &mut broadcast::Receiver<Pulse>| {
			let pulsed: Vec<Pulse> = std::iter::from_fn(|| pulses.try_recv().ok()).collect();
			pulsed.into_iter().any(|pulse| matches!(pulse, Pulse::Readings { node, .. } if node == "rdu"))
		};
		relay.observe(round());
		assert!(!readings(&mut pulses) && lent.try_recv().is_err());
		relay.watch("rdu");
		relay.observe(round());
		assert!(readings(&mut pulses) && lent.try_recv().is_err());
		relay.leased("tyo", Duration::from_secs(600));
		assert!(relay.leased_to("tyo"));
		relay.observe(round());
		assert_eq!(lent.try_recv().unwrap(), serde_json::json!({ "values": { "cpu.usage": 3.0 } }));
	}

	#[test]
	fn a_peer_not_heard_is_shown_waiting_with_nothing_held() {
		let peers = BTreeSet::from(["tyo".to_owned()]);
		let relay = Relay::new("rdu".into(), "s3cret".into(), peers, Store::memory().unwrap()).unwrap();
		let state = relay.state();
		let waiting =
			Entry { held: None, state: Presence::Waiting, runs: None, readings: None, round_trip: None };
		assert_eq!(state.nodes["tyo"], waiting);
	}

	#[test]
	fn a_state_time_alone_changes_is_sent_again_once() {
		let peers = BTreeSet::from(["tyo".to_owned(), "buf".to_owned()]);
		let relay = Relay::new("rdu".into(), "s3cret".into(), peers, Store::memory().unwrap()).unwrap();
		let now = Timestamp::now();
		let version = u64::try_from(now.as_millisecond()).unwrap();
		relay.take("tyo", Carried { version, snapshot: Arc::new(serde_json::json!({})) }, None);
		let mut changes = relay.subscribe_changes();
		let mut swept = |seconds: i64, uptime: u64| {
			relay.swept(now + jiff::SignedDuration::from_secs(seconds), Duration::from_secs(uptime));
			std::iter::from_fn(|| changes.try_recv().ok())
				.map(|change| (change.node, change.entry.state))
				.collect::<Vec<_>>()
		};
		assert_eq!(swept(5, 5), []);
		assert_eq!(swept(11, 11), [("tyo".to_owned(), Presence::Late)]);
		assert_eq!(swept(12, 12), []);
		// buf, never heard, is given up on once the relay has run a minute.
		let gone = [("buf".to_owned(), Presence::Gone), ("tyo".to_owned(), Presence::Gone)];
		assert_eq!(swept(61, 61), gone);
	}

	#[test]
	fn a_peer_named_is_shown_waiting_and_one_dropped_unheard_is_not_shown() {
		let peers = BTreeSet::from(["tyo".to_owned(), "buf".to_owned()]);
		let relay = Relay::new("rdu".into(), "s3cret".into(), peers, Store::memory().unwrap()).unwrap();
		let version = u64::try_from(Timestamp::now().as_millisecond()).unwrap();
		relay.take("tyo", Carried { version, snapshot: Arc::new(serde_json::json!({})) }, None);
		let mut changes = relay.subscribe_changes();
		relay.set_peers(BTreeSet::from(["sin".to_owned()]));
		let sent: Vec<_> = std::iter::from_fn(|| changes.try_recv().ok()).map(|c| c.node).collect();
		assert_eq!(sent, ["sin"]);
		let nodes = relay.state().nodes;
		let waiting =
			Entry { held: None, state: Presence::Waiting, runs: None, readings: None, round_trip: None };
		assert_eq!(nodes["sin"], waiting);
		// tyo, heard, is held until it ages; buf, never heard, is no longer listed.
		assert!(nodes.contains_key("tyo") && !nodes.contains_key("buf"));
	}
}
