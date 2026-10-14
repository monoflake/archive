//! A run: a task that has a workflow, its steps, the events sent to it and the lease it is held
//! under. Every statement here is the coordinator's half of RFC 23wgz; the executors' half is
//! `runs_api`. See spec/architecture/ledger.md, "A run, and the node that replays it".
//!
//! A write that settles something -- a grant, a step's result, a run's end -- commits confirmed, as
//! the cluster's default is. One a lost primary may lose without harm -- a lease's renewal, an
//! executor's heartbeat, an attempt's start -- commits `locally`. See RFC bhybp.

use super::Store;
use deadpool_postgres::GenericClient;
use jiff::Timestamp;

/// A step's result, at most this many bytes of JSON, Workflows' own limit. A larger one fails the
/// step. See RFC 23wgz.
pub const MAX_RESULT: usize = 1 << 20;

/// A lease lasts this long on the primary's clock and is renewed every `RENEW`, so a holder lost
/// mid-turn is found within half a minute. See RFC 23wgz.
pub const LEASE_NANOS: i64 = 30_000_000_000;
pub const RENEW_SECONDS: u64 = 10;

/// An executor not heard from within this is not given a turn: longer than a poll's 30 seconds.
const ALIVE_NANOS: i64 = 60_000_000_000;

/// A step started this many times by holders that were lost fails, so one that kills its isolate
/// cannot loop for ever. A crash is not an attempt. See RFC 23wgz.
pub const MAX_STARTS: i32 = 20;

/// A turn carries the results of its done steps up to this many bytes; past it `partial` is set
/// and the executor reads the rest one by one.
const TURN_BYTES: i64 = 8 << 20;

/// The primary's own clock, in nanoseconds, read once per statement or transaction: one clock
/// decides every expiry, never a client's. See RFC 23wgz.
const CLOCK: &str = "SELECT (extract(epoch FROM clock_timestamp()) * 1000000000)::bigint";

/// The same as an expression, for a statement that stamps and writes in one round trip.
const NOW: &str = "(extract(epoch FROM clock_timestamp()) * 1000000000)::bigint";

/// The lease a write claims: the run, the node that holds it and the number it was granted under.
#[derive(Debug, Clone, Copy)]
pub struct Lease<'a> {
	pub service: &'a str,
	pub run: &'a str,
	pub node: &'a str,
	pub fence: i64,
}

mod config;
mod grant;
mod open;
mod outcome;
mod record;
mod steps;
mod waits;

pub use config::{Backoff, Class, Kind, NewRun, Retries, StepConfig};
pub use grant::{Granted, Turn};
pub use open::{Created, LeaseRow, RunRow, RunView, StepRow};
pub use steps::{Begun, Finished, Outcome};
pub use waits::{Slept, Waited};

pub(crate) fn timestamp(at: i64) -> Timestamp {
	Timestamp::from_nanosecond(i128::from(at)).unwrap_or(Timestamp::UNIX_EPOCH)
}

/// How a write that needs the lease ended when it did not.
#[derive(Debug, PartialEq)]
pub enum Refused {
	NoSuchRun,
	/// The fence or the holder is not the run's current one.
	Stale,
	/// The step is not the kind or in the state the record says.
	Diverged(String),
}

/// What `run` and `view` read, a run's whole standing, so a test and the console see one thing.
type Checked<T> = Result<T, Refused>;

impl Store {
	async fn clock(client: &impl GenericClient) -> anyhow::Result<i64> {
		Ok(client.query_one(CLOCK, &[]).await?.get(0))
	}
}

fn stage(name: &str) -> String {
	format!("step.{name}")
}

fn millis(ms: u64) -> i64 {
	i64::try_from(ms).unwrap_or(i64::MAX).saturating_mul(1_000_000)
}

#[cfg(test)]
mod tests;
