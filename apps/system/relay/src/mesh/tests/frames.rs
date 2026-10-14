//! Compressed frames, and a corrupt one.

use super::*;

/// A socket whose every frame written is kept as it was written, compressed or not.
struct Raw {
	incoming: mpsc::UnboundedReceiver<Received>,
	frames: Arc<Mutex<Vec<Frame>>>,
}

struct Recorded(Arc<Mutex<Vec<Frame>>>);

impl Writer for Recorded {
	async fn send(&mut self, frame: Frame) -> Result<(), SocketError> {
		self.0.lock().unwrap().push(frame);
		Ok(())
	}
}

impl Halves for Raw {
	type Reader = Fed;
	type Writer = Recorded;

	fn halves(self) -> (Fed, Recorded) {
		(Fed(self.incoming), Recorded(self.frames))
	}
}

/// tyo's hello, saying it reads compressed frames where `zstd`.
fn hello_from_tyo(zstd: Option<bool>) -> Received {
	let hello = Message::Hello {
		version: VERSION,
		node: "tyo".into(),
		versions: Versions::new(),
		runs: None,
		minutes: None,
		beats: Some(true),
		watch: None,
		zstd,
		sync: None,
	};
	Received::Text(serde_json::to_string(&hello).unwrap())
}

#[tokio::test]
async fn a_neighbor_that_reads_compressed_frames_is_sent_them_and_one_that_does_not_plain() {
	for (zstd, packed) in [(Some(true), true), (None, false)] {
		let relay = relay("rdu");
		let snapshot = Arc::new(serde_json::json!({ "apps": [{ "name": "geo" }] }));
		relay.take("rdu", Carried { version: 9, snapshot }, None);
		let (feed, incoming) = mpsc::unbounded_channel();
		feed.send(hello_from_tyo(zstd)).unwrap();
		drop(feed);
		let frames = Arc::new(Mutex::new(Vec::new()));
		let raw = Heard::new(Raw { incoming, frames: frames.clone() });
		converse(relay, Some("tyo"), raw).await.unwrap();
		let frames = frames.lock().unwrap();
		// The hello goes plain either way: it is what says whether the other end reads them.
		assert!(matches!(&frames[0], Frame::Text(hello) if hello.contains("\"zstd\":true")));
		let mut unpacker = Unpacker::new().unwrap();
		let sent: Vec<Message> = frames[1..]
			.iter()
			.map(|frame| match frame {
				Frame::Binary(bytes) if packed => {
					serde_json::from_slice(&unpacker.unpack(bytes).unwrap()).unwrap()
				}
				Frame::Text(text) if !packed => serde_json::from_str(text).unwrap(),
				other => panic!("{zstd:?}: {other:?}"),
			})
			.collect();
		assert!(
			matches!(&sent[0], Message::Node { node, state } if node == "rdu" && state.version == 9)
		);
	}
}

#[tokio::test]
async fn a_corrupt_frame_ends_the_conversation_and_the_next_starts_fresh() {
	let relay = relay("rdu");
	let (feed, incoming) = mpsc::unbounded_channel();
	feed.send(hello_from_tyo(Some(true))).unwrap();
	feed.send(Received::Binary(Bytes::from_static(b"no zstd stream"))).unwrap();
	let raw = Heard::new(Raw { incoming, frames: Arc::default() });
	let ended = converse(relay.clone(), Some("tyo"), raw).await;
	assert!(matches!(ended, Err(MeshError::Corrupt(_))), "{ended:?}");
	drop(feed);

	// Redialed: new contexts both ways, and the first compressed frame reads at once.
	let (feed, incoming) = mpsc::unbounded_channel();
	feed.send(hello_from_tyo(Some(true))).unwrap();
	let beat = Message::Beat { node: "tyo".into(), at: 1_791_999_000_000 };
	let packed = Packer::new().unwrap().pack(serde_json::to_string(&beat).unwrap().as_bytes());
	feed.send(Received::Binary(packed.unwrap().into())).unwrap();
	drop(feed);
	let raw = Heard::new(Raw { incoming, frames: Arc::default() });
	converse(relay.clone(), Some("tyo"), raw).await.unwrap();
	assert_eq!(relay.beats().get("tyo"), Some(&1_791_999_000_000));
}
