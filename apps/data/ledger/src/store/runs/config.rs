//! What a run and a step are asked to be: a step's retries and timeout, and a run's class.

use ledger::Caller;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// What a run is told about a step when it is first asked for: Workflows' own defaults. See
/// https://developers.cloudflare.com/workflows/build/sleeping-and-retrying/ -- `retries.limit` 5
/// retries after the first attempt, `delay` 10 seconds, `backoff` exponential, `timeout` 10
/// minutes.
const DEFAULT_LIMIT: u32 = 5;
const DEFAULT_DELAY_MS: u64 = 10_000;
const DEFAULT_TIMEOUT_MS: u64 = 600_000;
/// Workflows limits a step to 10,000 retries.
const MAX_LIMIT: u32 = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Backoff {
	Constant,
	Linear,
	Exponential,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Retries {
	pub limit: u32,
	pub delay_ms: u64,
	pub backoff: Backoff,
}

impl Default for Retries {
	fn default() -> Self {
		Self { limit: DEFAULT_LIMIT, delay_ms: DEFAULT_DELAY_MS, backoff: Backoff::Exponential }
	}
}

/// A step's `retries` and `timeout`, as the code declared them, a missing part taken from the
/// defaults. Kept whole in the step's row, so the schedule of a retry never changes mid-way.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepConfig {
	#[serde(default)]
	pub retries: Retries,
	#[serde(default = "default_timeout")]
	pub timeout_ms: u64,
}

fn default_timeout() -> u64 {
	DEFAULT_TIMEOUT_MS
}

impl Default for StepConfig {
	fn default() -> Self {
		Self { retries: Retries::default(), timeout_ms: DEFAULT_TIMEOUT_MS }
	}
}

impl StepConfig {
	pub fn valid(&self) -> bool {
		self.retries.limit <= MAX_LIMIT && self.timeout_ms >= 1
	}

	/// How long to wait before attempt `failed + 1`, in milliseconds.
	pub fn delay_after(&self, failed: i32) -> u64 {
		let n = u64::try_from(failed.max(1)).unwrap_or(1);
		let base = self.retries.delay_ms;
		match self.retries.backoff {
			Backoff::Constant => base,
			Backoff::Linear => base.saturating_mul(n),
			Backoff::Exponential => base.saturating_mul(1u64 << (n - 1).min(32)),
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Class {
	Quorum,
	Local,
}

impl Class {
	pub(super) fn word(self) -> &'static str {
		match self {
			Self::Quorum => "quorum",
			Self::Local => "local",
		}
	}
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
	Do,
	Sleep,
	Wait,
}

impl Kind {
	pub(super) fn word(self) -> &'static str {
		match self {
			Self::Do => "do",
			Self::Sleep => "sleep",
			Self::Wait => "wait",
		}
	}
}

/// What `PUT /v1/runs/{service}/{id}` asks.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct NewRun {
	pub workflow: String,
	#[serde(default = "quorum")]
	pub class: Class,
	#[serde(default)]
	pub params: Value,
	#[serde(default = "ours")]
	pub caller: Caller,
}

fn quorum() -> Class {
	Class::Quorum
}

fn ours() -> Caller {
	Caller::Ours
}
