//! A neighbor that reads nothing, and two that both write faster than they read.

use super::*;

/// A socket to a neighbor gone without closing: the hello goes, and every write after it waits on
/// a buffer nobody drains, as one does once the kernel's is full.
struct Stuck {
	incoming: mpsc::UnboundedReceiver<Received>,
}

/// The writing half of `Stuck`.
struct Waiting {
	written: usize,
}

impl Writer for Waiting {
	async fn send(&mut self, _: Frame) -> Result<(), SocketError> {
		self.written += 1;
		if self.written > 1 {
			std::future::pending::<()>().await;
		}
		Ok(())
	}
}

impl Halves for Stuck {
	type Reader = Fed;
	type Writer = Waiting;

	fn halves(self) -> (Fed, Waiting) {
		(Fed(self.incoming), Waiting { written: 0 })
	}
}

#[tokio::test]
async fn a_neighbor_that_takes_nothing_written_is_dropped_once_silent() {
	let relay = relay("0000000000000007");
	let (feed, incoming) = mpsc::unbounded_channel();
	let hello = Message::Hello {
		version: VERSION,
		node: "0000000000000001".into(),
		versions: Versions::new(),
		runs: None,
		minutes: None,
		beats: None,
		watch: None,
		zstd: None,
		sync: None,
	};
	feed.send(Received::Text(serde_json::to_string(&hello).unwrap())).unwrap();
	let silence = Duration::from_millis(300);
	let stuck = Heard::within(Stuck { incoming }, silence);
	let opened = Instant::now();
	let conversing = tokio::spawn(converse(relay.clone(), Some("0000000000000001"), stuck));
	// This node's next snapshot, which the socket starts to write and never finishes.
	relay.take(
		"0000000000000007",
		Carried { version: 1, snapshot: Arc::new(serde_json::json!({})) },
		None,
	);
	let ended = tokio::time::timeout(Duration::from_secs(5), conversing)
		.await
		.expect("dropped, not left waiting on the write")
		.unwrap();
	// Whichever comes first: the reading silent, or the write waiting as long.
	let given_up = |error: &SocketError| error.silent() || error.stalled();
	assert!(matches!(ended, Err(MeshError::Socket(error)) if given_up(&error)));
	assert!(opened.elapsed() >= silence);
	drop(feed);
}

/// Smaller than one snapshot, so a write fills the other end's buffer long before it is read.
const NARROW: u32 = 4 * 1024;

/// Two ends of one socket on loopback, each with buffers of `NARROW`: the one dialed, and the one
/// accepted, which takes the listener's.
async fn narrow() -> (Dial, Dial) {
	let narrowed = || {
		let socket = tokio::net::TcpSocket::new_v4().unwrap();
		socket.set_send_buffer_size(NARROW).unwrap();
		socket.set_recv_buffer_size(NARROW).unwrap();
		socket
	};
	let listening = narrowed();
	listening.bind("127.0.0.1:0".parse().unwrap()).unwrap();
	let listener = listening.listen(1).unwrap();
	let address = listener.local_addr().unwrap();
	let (dialed, accepted) = tokio::join!(narrowed().connect(address), listener.accept());
	let (dialed, (accepted, _)) = (dialed.unwrap(), accepted.unwrap());
	let url = format!("ws://{address}{PATH}");
	let (client, server) = tokio::join!(
		tokio_tungstenite::client_async(url, MaybeTlsStream::Plain(dialed)),
		tokio_tungstenite::accept_async(MaybeTlsStream::Plain(accepted)),
	);
	(client.unwrap().0, server.unwrap())
}

/// What jammed `sin` and `nrt` on 2026-10-14: both ends writing faster than the other reads, so
/// each waits on a write the other never reads, since it too is waiting on one. Each must still
/// read, and so each still hears the other's newest once the writing stops.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn two_ends_writing_faster_than_either_reads_keep_reading() {
	let (here, there) = (relay("0000000000000007"), relay("0000000000000009"));
	let (dialed, accepted) = narrow().await;
	let silence = Duration::from_secs(10);
	let dialing =
		tokio::spawn(converse(here.clone(), Some("0000000000000009"), Heard::within(dialed, silence)));
	let accepting = tokio::spawn(converse(there.clone(), None, Heard::within(accepted, silence)));

	let snapshot = Arc::new(serde_json::json!({ "said": "x".repeat(1024 * 1024) }));
	let mut last = 0;
	let writing = Instant::now();
	while writing.elapsed() < Duration::from_secs(1) {
		last += 1;
		here.take("0000000000000007", Carried { version: last, snapshot: snapshot.clone() }, None);
		there.take("0000000000000009", Carried { version: last, snapshot: snapshot.clone() }, None);
		tokio::time::sleep(Duration::from_millis(10)).await;
	}
	let caught = async {
		while here.versions().get("0000000000000009") != Some(&last)
			|| there.versions().get("0000000000000007") != Some(&last)
		{
			tokio::time::sleep(Duration::from_millis(50)).await;
		}
	};
	let held = tokio::time::timeout(Duration::from_secs(20), caught).await;
	let (finished, ended) = (dialing.is_finished(), accepting.is_finished());
	dialing.abort();
	accepting.abort();
	assert!(held.is_ok(), "each holds the other's newest, {last}");
	assert!(!finished && !ended, "both sockets still open");
}
