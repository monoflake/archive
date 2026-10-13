//! The kernel's own figure for a TCP connection's round trip, from Linux's `TCP_INFO`: the least
//! RTT it saw in its window, `tcpi_min_rtt`, or the smoothed one, `tcpi_rtt`, from a kernel older
//! than 4.6. Both are timed on the connection's ACKs beneath every queue of the relay's; the least
//! leaves out the delayed ACKs a mostly one-way socket draws, which the smoothed one counts.
//! Elsewhere there is none, and the mesh's timed pings stand. See spec/architecture/relay.md, "The
//! round trip to each neighbor".
//!
//! Only `std`, so the Linux half can be built and tested on its own in a Linux container.

use std::os::fd::AsFd;
use std::time::Duration;

/// `struct tcp_info`, as `u32` words, from include/uapi/linux/tcp.h: eight bytes of flags, then
/// `u32`s to `tcpi_total_retrans` at byte 100, four `u64`s from byte 104, and `u32`s again.
/// `tcpi_rtt` is at byte 68, `tcpi_min_rtt` at byte 148, after `tcpi_notsent_bytes`.
#[cfg(any(target_os = "linux", test))]
const RTT: usize = 17;
#[cfg(any(target_os = "linux", test))]
const MIN_RTT: usize = 37;

/// Room past the struct's 280 bytes as of Linux 7.0; a kernel fills as much as it has, never more
/// than it is given, and says how much.
#[cfg(any(target_os = "linux", test))]
const WORDS: usize = 96;

/// The round trip `info`, `filled` bytes of it written by the kernel, holds: the least where the
/// kernel wrote it and has a sample, else the smoothed, else none.
#[cfg(any(target_os = "linux", test))]
fn chosen(info: &[u32; WORDS], filled: usize) -> Option<Duration> {
	let held = |word: usize| (filled >= (word + 1) * 4 && info[word] != 0).then_some(info[word]);
	let micros = held(MIN_RTT).or_else(|| held(RTT))?;
	Some(Duration::from_micros(micros.into()))
}

/// `TCP_INFO` of the connection `socket` is, and how many bytes of it the kernel wrote.
#[cfg(target_os = "linux")]
fn info(socket: &impl AsFd) -> Option<([u32; WORDS], usize)> {
	use std::ffi::{c_int, c_void};
	use std::os::fd::AsRawFd;

	// glibc's and musl's alike; the values are Linux's own, on every architecture.
	unsafe extern "C" {
		fn getsockopt(
			socket: c_int,
			level: c_int,
			name: c_int,
			value: *mut c_void,
			length: *mut u32,
		) -> c_int;
	}
	const IPPROTO_TCP: c_int = 6;
	const TCP_INFO: c_int = 11;
	let mut info = [0u32; WORDS];
	let mut length = u32::try_from(std::mem::size_of_val(&info)).ok()?;
	let fd = socket.as_fd().as_raw_fd();
	// SAFETY: `info` is writable for `length` bytes, and the kernel writes at most that many and
	// says how many in `length`.
	let read =
		unsafe { getsockopt(fd, IPPROTO_TCP, TCP_INFO, info.as_mut_ptr().cast(), &raw mut length) };
	(read == 0).then_some((info, usize::try_from(length).ok()?))
}

/// The round trip of the connection `socket` is, or none where it cannot be read or holds no
/// sample yet.
#[cfg(target_os = "linux")]
pub fn round_trip(socket: &impl AsFd) -> Option<Duration> {
	let (info, filled) = info(socket)?;
	chosen(&info, filled)
}

#[cfg(not(target_os = "linux"))]
pub fn round_trip(_: &impl AsFd) -> Option<Duration> {
	None
}

#[cfg(test)]
mod tests {
	use super::*;
	use std::io::{Read, Write};
	use std::net::{TcpListener, TcpStream};

	/// A connection that has carried a few exchanges both ways, its two ends.
	fn exchanged() -> (TcpStream, TcpStream) {
		let listener = TcpListener::bind("127.0.0.1:0").unwrap();
		let mut dialed = TcpStream::connect(listener.local_addr().unwrap()).unwrap();
		let (mut accepted, _) = listener.accept().unwrap();
		let mut byte = [0u8; 1];
		for _ in 0..4 {
			dialed.write_all(b"p").unwrap();
			accepted.read_exact(&mut byte).unwrap();
			accepted.write_all(b"q").unwrap();
			dialed.read_exact(&mut byte).unwrap();
		}
		(dialed, accepted)
	}

	#[test]
	fn the_least_is_taken_where_the_kernel_wrote_it_and_the_smoothed_where_not() {
		let mut info = [0u32; WORDS];
		info[RTT] = 20_400;
		info[MIN_RTT] = 950;
		assert_eq!(chosen(&info, 248), Some(Duration::from_micros(950)));
		assert_eq!(chosen(&info, (MIN_RTT + 1) * 4), Some(Duration::from_micros(950)));
		// A kernel older than 4.6 stops short of it: what lies past what it wrote is not read.
		assert_eq!(chosen(&info, MIN_RTT * 4), Some(Duration::from_micros(20_400)));
		assert_eq!(chosen(&info, 104), Some(Duration::from_micros(20_400)));
		assert_eq!(chosen(&info, RTT * 4), None);
		// No least sampled yet.
		info[MIN_RTT] = 0;
		assert_eq!(chosen(&info, 248), Some(Duration::from_micros(20_400)));
		info[RTT] = 0;
		assert_eq!(chosen(&info, 248), None);
	}

	#[cfg(target_os = "linux")]
	#[test]
	fn a_connection_on_linux_reads_its_least_round_trip() {
		let (dialed, accepted) = exchanged();
		for end in [&dialed, &accepted] {
			let (info, filled) = super::info(end).expect("TCP_INFO read");
			assert!(filled >= (MIN_RTT + 1) * 4 && filled <= WORDS * 4, "{filled} bytes");
			let (least, smoothed) = (info[MIN_RTT], info[RTT]);
			assert!(least > 0 && smoothed > 0 && least < 1_000_000, "{least} {smoothed}");
			assert_eq!(round_trip(end), Some(Duration::from_micros(least.into())));
		}
	}

	/// What is not a TCP socket has no figure, and the pings stand.
	#[test]
	fn a_file_has_none() {
		let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
		assert_eq!(round_trip(&file), None);
	}

	#[cfg(not(target_os = "linux"))]
	#[test]
	fn off_linux_there_is_none() {
		let (dialed, _accepted) = exchanged();
		assert_eq!(round_trip(&dialed), None);
	}
}
