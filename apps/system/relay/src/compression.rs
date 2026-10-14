//! The mesh's frames compressed: a zstd stream per direction of each connection, flushed at the end
//! of every frame, so a frame decodes as soon as it arrives, against everything its stream carried
//! before it. Only a neighbor whose hello says it reads them is sent compressed frames. See
//! spec/architecture/relay.md, RFC grmtb.

use std::io;
use zstd_safe::zstd_sys::ZSTD_EndDirective;
use zstd_safe::{CCtx, CParameter, DCtx, DParameter, ErrorCode, InBuffer, OutBuffer};

/// zstd's own default: a frame of a few hundred bytes takes microseconds.
const LEVEL: i32 = 3;

/// The window, as a power of two: 128 KiB of what a stream carried before, room for every node's
/// snapshot many times over, and the most a neighbor's stream may make this one hold. Twice as
/// much compressed recorded rounds no better.
pub const WINDOW_LOG: u32 = 17;

/// The match tables, as powers of two of their entries: 128 KiB each.
const HASH_LOG: u32 = 15;

/// The most one frame may unpack to, so a broken stream cannot take the relay's memory: the
/// socket's own limit on a message.
const MOST: usize = 64 << 20;

fn failed(code: ErrorCode) -> io::Error {
	io::Error::other(zstd_safe::get_error_name(code))
}

/// One direction's compressor.
pub struct Packer(CCtx<'static>);

impl Packer {
	pub fn new() -> io::Result<Self> {
		let mut context = CCtx::create();
		context.set_parameter(CParameter::CompressionLevel(LEVEL)).map_err(failed)?;
		context.set_parameter(CParameter::WindowLog(WINDOW_LOG)).map_err(failed)?;
		// The level's own tables are sized for megabytes of input; a window of 128 KiB needs less.
		context.set_parameter(CParameter::HashLog(HASH_LOG)).map_err(failed)?;
		context.set_parameter(CParameter::ChainLog(HASH_LOG)).map_err(failed)?;
		Ok(Self(context))
	}

	/// `frame` compressed and flushed, ready to be sent whole.
	pub fn pack(&mut self, frame: &[u8]) -> io::Result<Vec<u8>> {
		let mut packed = Vec::with_capacity(frame.len() / 2 + 64);
		let mut input = InBuffer::around(frame);
		loop {
			if packed.capacity() - packed.len() < 1024 {
				packed.reserve(frame.len().max(1024));
			}
			let written = packed.len();
			let mut output = OutBuffer::around_pos(&mut packed, written);
			let left = self
				.0
				.compress_stream2(&mut output, &mut input, ZSTD_EndDirective::ZSTD_e_flush)
				.map_err(failed)?;
			if left == 0 && input.pos() == frame.len() {
				return Ok(packed);
			}
		}
	}

	/// What the context holds, in bytes.
	pub fn size(&self) -> usize {
		self.0.sizeof()
	}
}

/// One direction's decompressor.
pub struct Unpacker(DCtx<'static>);

impl Unpacker {
	pub fn new() -> io::Result<Self> {
		let mut context = DCtx::create();
		context.set_parameter(DParameter::WindowLogMax(WINDOW_LOG)).map_err(failed)?;
		Ok(Self(context))
	}

	/// `packed`, one whole frame of its stream, as it was before it was packed.
	pub fn unpack(&mut self, packed: &[u8]) -> io::Result<Vec<u8>> {
		let mut frame = Vec::with_capacity(packed.len() * 4 + 256);
		let mut input = InBuffer::around(packed);
		loop {
			if frame.capacity() - frame.len() < 4096 {
				if frame.len() >= MOST {
					return Err(io::Error::other("a frame unpacks past the most one may"));
				}
				frame.reserve(frame.capacity().max(4096));
			}
			let written = frame.len();
			let mut output = OutBuffer::around_pos(&mut frame, written);
			self.0.decompress_stream(&mut output, &mut input).map_err(failed)?;
			let room = output.pos() < output.capacity();
			if input.pos() == packed.len() && room {
				return Ok(frame);
			}
		}
	}

	/// What the context holds, in bytes.
	pub fn size(&self) -> usize {
		self.0.sizeof()
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	/// A snapshot as a relay sends one, its apps `apps` long.
	fn snapshot(version: u64, apps: usize) -> Vec<u8> {
		let apps: Vec<_> = (0..apps)
			.map(|n| {
				serde_json::json!({ "name": format!("app{n}"), "image": format!("sha256:{n:064}"),
					"deployed_at": "2026-10-14T12:00:00Z", "running": true, "held": false,
					"rollout": "replace" })
			})
			.collect();
		let state = serde_json::json!({ "version": version, "snapshot": { "apps": apps } });
		serde_json::to_vec(&serde_json::json!({ "type": "node", "node": "rdu", "state": state }))
			.unwrap()
	}

	#[test]
	fn each_frame_decodes_as_it_arrives_and_the_next_like_it_costs_little() {
		let (mut packer, mut unpacker) = (Packer::new().unwrap(), Unpacker::new().unwrap());
		let first = snapshot(1, 20);
		let packed = packer.pack(&first).unwrap();
		assert_eq!(unpacker.unpack(&packed).unwrap(), first);
		let next = snapshot(2, 20);
		let again = packer.pack(&next).unwrap();
		assert_eq!(unpacker.unpack(&again).unwrap(), next);
		assert!(again.len() * 20 < next.len(), "{} of {}", again.len(), next.len());
	}

	#[test]
	fn a_corrupt_frame_is_refused() {
		let mut unpacker = Unpacker::new().unwrap();
		assert!(unpacker.unpack(b"not a zstd stream at all").is_err());
	}

	#[test]
	fn a_connections_two_contexts_stay_within_a_bound() {
		let (mut packer, mut unpacker) = (Packer::new().unwrap(), Unpacker::new().unwrap());
		for version in 0..50 {
			let packed = packer.pack(&snapshot(version, 40)).unwrap();
			unpacker.unpack(&packed).unwrap();
		}
		let (packing, unpacking) = (packer.size(), unpacker.size());
		eprintln!("packer {packing} bytes, unpacker {unpacking} bytes");
		// 1,173,009 and 620,320 bytes measured on 2026-10-14.
		assert!(packing + unpacking < 2 << 20, "{packing} + {unpacking}");
	}
}
