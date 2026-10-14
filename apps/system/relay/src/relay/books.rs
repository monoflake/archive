//! A relay's books: the runs mirrored and the minutes, taken, asked for and written.

use super::{Relay, Runs, VERSION, lock};
use crate::cluster::{Carried, Versions};
use crate::history::DAY;
use crate::history::answer::{Asked, History};
use crate::runs::{Batch, StoreError};
use jiff::Timestamp;
use tokio::sync::broadcast;

impl Relay {
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
}
