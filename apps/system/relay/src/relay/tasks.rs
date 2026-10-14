//! What a relay runs beside its sockets: reading host, recalling its runs, and writing.

use super::Relay;
use crate::history::minute_of;
use crate::host::{PAGE, Reader};
use jiff::Timestamp;
use std::sync::Arc;
use std::time::Duration;

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
