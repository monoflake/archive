//! What a `Ledger` hands over, sent from one background task: batched to `POST {base}/events`, held
//! in a bounded queue while the ledger cannot be reached and tried again with backoff, and sent at
//! once, past any backoff, when the service stopping asks it to flush. Each way a record is held or
//! lost is said once per streak, never the body it carries. See spec/architecture/ledger.md.

use crate::{BACKOFF, BATCH, Item, WAITING};
use bytes::Bytes;
use http_body_util::Full;
use hyper::{Method, Request};
use hyper_util::client::legacy::Client;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use std::collections::VecDeque;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tokio::sync::{mpsc, oneshot};
use tokio::time::Instant;

/// Where the lines saying what happened to the records go: stderr, as every service logs, or a
/// test's own capture.
pub(crate) type Log = Arc<dyn Fn(&str) + Send + Sync>;

/// What a `Ledger` hands its background task: an item to send, or a flush to answer by `deadline`
/// with how many items it still holds.
pub(crate) enum Message {
	Item(Box<Item>),
	Flush { deadline: Instant, answer: oneshot::Sender<usize> },
}

/// How long one `POST` may take before the ledger counts as not answering.
const PATIENCE: Duration = Duration::from_secs(10);

/// How one batch went.
#[derive(Debug, Clone, PartialEq, Eq)]
enum Sent {
	/// The ledger took it.
	Taken,
	/// Answered, and refused: the ledger's word on this batch, which sending it again would not
	/// change, so it is dropped. Said by the answer's status, or what kept it from being sent.
	Refused(String),
	/// Not answered, or a server's failure: held, and tried again. Said by why.
	Unanswered(String),
}

/// Keep at most `WAITING`, the oldest going first; how many went.
pub(crate) fn keep(waiting: &mut VecDeque<Item>, item: Item) -> usize {
	waiting.push_back(item);
	let mut dropped = 0;
	while waiting.len() > WAITING {
		waiting.pop_front();
		dropped += 1;
	}
	dropped
}

/// The background task's state: what waits, and the streaks a line has been said for.
struct Deliverer {
	base: String,
	client: Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>,
	waiting: VecDeque<Item>,
	/// How many wait, for a flush that is not answered within its bound to read.
	held: Arc<AtomicUsize>,
	backoff: Duration,
	log: Log,
	/// Since the first unanswered send of a streak.
	unanswered: bool,
	/// Items refused since the first refusal of a streak.
	refused: usize,
	/// Items dropped, the oldest, since the first of a streak.
	dropped: usize,
}

impl Deliverer {
	fn keep(&mut self, item: Item) {
		let dropped = keep(&mut self.waiting, item);
		if dropped > 0 && self.dropped == 0 {
			let base = &self.base;
			(self.log)(&format!(
				"ledger: more than {WAITING} items wait for {base}; dropping the oldest until it answers"
			));
		}
		self.dropped += dropped;
		self.held.store(self.waiting.len(), Ordering::Relaxed);
	}

	/// One batch sent, its streaks said once and ended once; whether the ledger answered at all.
	async fn attempt(&mut self, deadline: Option<Instant>) -> bool {
		let batch: Vec<&Item> = self.waiting.iter().take(BATCH).collect();
		let count = batch.len();
		let sent = send(&self.client, &self.base, &batch, deadline).await;
		let base = self.base.clone();
		let answered = match sent {
			Sent::Taken => {
				if self.unanswered {
					(self.log)(&format!("ledger: {base} answered again; {count} items sent"));
					self.unanswered = false;
				}
				if self.refused > 0 {
					let refused = self.refused;
					(self.log)(&format!("ledger: {base} takes items again, after refusing {refused}"));
					self.refused = 0;
				}
				true
			}
			Sent::Refused(why) => {
				if self.refused == 0 {
					(self.log)(&format!("ledger: {base} refused {count} items with {why}; dropped"));
				}
				self.refused += count;
				true
			}
			Sent::Unanswered(why) => {
				if !self.unanswered {
					let held = self.waiting.len();
					(self.log)(&format!(
						"ledger: {base} did not answer ({why}); holding {held} items and trying again"
					));
					self.unanswered = true;
				}
				false
			}
		};
		if answered {
			self.waiting.drain(..count);
			self.backoff = BACKOFF.0;
			if self.dropped > 0 && self.waiting.len() < WAITING {
				let dropped = self.dropped;
				(self.log)(&format!("ledger: {dropped} items for {base} were dropped, the oldest"));
				self.dropped = 0;
			}
		}
		self.held.store(self.waiting.len(), Ordering::Relaxed);
		answered
	}

	/// Everything held sent now, past any backoff, until it is sent, the ledger does not answer, or
	/// `deadline` passes; how many are still held.
	async fn flush(
		&mut self,
		receiver: &mut mpsc::UnboundedReceiver<Message>,
		deadline: Instant,
	) -> usize {
		self.drain(receiver);
		while !self.waiting.is_empty() && Instant::now() < deadline {
			if !self.attempt(Some(deadline)).await {
				break;
			}
		}
		self.waiting.len()
	}

	/// Every item already handed over, kept; a flush asked meanwhile is answered with what is held
	/// so far, the one being answered covering it.
	fn drain(&mut self, receiver: &mut mpsc::UnboundedReceiver<Message>) {
		while let Ok(message) = receiver.try_recv() {
			match message {
				Message::Item(item) => self.keep(*item),
				Message::Flush { answer, .. } => {
					let _ = answer.send(self.waiting.len());
				}
			}
		}
	}
}

pub(crate) async fn deliver(
	base: String,
	mut receiver: mpsc::UnboundedReceiver<Message>,
	held: Arc<AtomicUsize>,
	log: Log,
) {
	// Roots compiled in rather than read from the system: an image built from scratch has none. The
	// provider is named rather than left to the build's features, which may enable two.
	let https = hyper_rustls::HttpsConnectorBuilder::new()
		.with_provider_and_webpki_roots(rustls::crypto::ring::default_provider())
		.expect("ring supports TLS 1.2 and 1.3")
		.https_or_http()
		.enable_http1()
		.build();
	let client = Client::builder(TokioExecutor::new()).build(https);
	let mut deliverer = Deliverer {
		base,
		client,
		waiting: VecDeque::new(),
		held,
		backoff: BACKOFF.0,
		log,
		unanswered: false,
		refused: 0,
		dropped: 0,
	};
	loop {
		if deliverer.waiting.is_empty() {
			match receiver.recv().await {
				Some(Message::Item(item)) => deliverer.keep(*item),
				Some(Message::Flush { answer, .. }) => {
					let _ = answer.send(0);
					continue;
				}
				None => return,
			}
		}
		while let Ok(message) = receiver.try_recv() {
			match message {
				Message::Item(item) => deliverer.keep(*item),
				Message::Flush { deadline, answer } => {
					let held = deliverer.flush(&mut receiver, deadline).await;
					let _ = answer.send(held);
				}
			}
		}
		if deliverer.waiting.is_empty() || deliverer.attempt(None).await {
			continue;
		}
		// Waiting out the backoff, while what arrives meanwhile still queues, and a flush asked of a
		// stopping service is answered at once rather than after it.
		let until = Instant::now() + deliverer.backoff;
		loop {
			tokio::select! {
				() = tokio::time::sleep_until(until) => break,
				arrived = receiver.recv() => match arrived {
					Some(Message::Item(item)) => deliverer.keep(*item),
					Some(Message::Flush { deadline, answer }) => {
						let held = deliverer.flush(&mut receiver, deadline).await;
						let _ = answer.send(held);
						break;
					}
					None => return,
				},
			}
		}
		deliverer.backoff = (deliverer.backoff * 2).min(BACKOFF.1);
	}
}

/// One batch `POST`ed, within `PATIENCE` and, for a flush, before `deadline`.
async fn send(
	client: &Client<hyper_rustls::HttpsConnector<HttpConnector>, Full<Bytes>>,
	base: &str,
	batch: &[&Item],
	deadline: Option<Instant>,
) -> Sent {
	let Ok(body) = serde_json::to_vec(batch) else {
		return Sent::Refused("a body that could not be written".into());
	};
	let Ok(request) = Request::builder()
		.method(Method::POST)
		.uri(format!("{base}/events"))
		.header("content-type", "application/json")
		.body(Full::new(Bytes::from(body)))
	else {
		return Sent::Refused("a request that could not be formed".into());
	};
	let patience = Instant::now() + PATIENCE;
	let until = deadline.map_or(patience, |deadline| deadline.min(patience));
	match tokio::time::timeout_at(until, client.request(request)).await {
		Ok(Ok(answer)) if answer.status().is_success() => Sent::Taken,
		// A refusal is the ledger's answer about this batch, and sending it again would not change
		// it; only a failure to reach the ledger is tried again.
		Ok(Ok(answer)) if !answer.status().is_server_error() => {
			Sent::Refused(answer.status().as_u16().to_string())
		}
		Ok(Ok(answer)) => Sent::Unanswered(format!("it answered {}", answer.status().as_u16())),
		Ok(Err(error)) => Sent::Unanswered(error.to_string()),
		Err(_) => Sent::Unanswered("no answer in time".into()),
	}
}

#[cfg(test)]
mod tests;
