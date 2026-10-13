//! The kernel's own figure for a TCP connection's round trip: Linux's smoothed RTT, from
//! `TCP_INFO`. It is timed on the connection's ACKs beneath every queue of the relay's, so a socket
//! busy sending snapshots does not count its own backlog. Elsewhere there is none, and the mesh's
//! timed pings stand. See spec/architecture/relay.md, "The round trip to each neighbor".
//!
//! Only `std`, so the Linux half can be built and tested on its own in a Linux container.

use std::os::fd::AsFd;
use std::time::Duration;

/// The smoothed round trip of the connection `socket` is, or none where it cannot be read or holds
/// no sample yet.
#[cfg(target_os = "linux")]
pub fn smoothed(socket: &impl AsFd) -> Option<Duration> {
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
	// `struct tcp_info` is eight bytes of flags and then `u32`s: `tcpi_rtt`, in microseconds, is
	// the eighteenth, at byte 68. The 26 words are the struct as Linux 2.6 first had it; a newer
	// kernel fills as much as it is given.
	const RTT: usize = 17;
	let mut info = [0u32; 26];
	let mut length = u32::try_from(std::mem::size_of_val(&info)).ok()?;
	let fd = socket.as_fd().as_raw_fd();
	// SAFETY: `info` is writable for `length` bytes, and the kernel writes at most that many and
	// says how many in `length`.
	let read =
		unsafe { getsockopt(fd, IPPROTO_TCP, TCP_INFO, info.as_mut_ptr().cast(), &raw mut length) };
	let filled = usize::try_from(length).ok()?;
	if read != 0 || filled < (RTT + 1) * 4 || info[RTT] == 0 {
		return None;
	}
	Some(Duration::from_micros(info[RTT].into()))
}

#[cfg(not(target_os = "linux"))]
pub fn smoothed(_: &impl AsFd) -> Option<Duration> {
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

	#[cfg(target_os = "linux")]
	#[test]
	fn a_connection_on_linux_has_a_smoothed_round_trip() {
		let (dialed, accepted) = exchanged();
		for end in [&dialed, &accepted] {
			let round_trip = smoothed(end).expect("TCP_INFO read");
			assert!(round_trip > Duration::ZERO && round_trip < Duration::from_secs(1), "{round_trip:?}");
		}
	}

	/// What is not a TCP socket has no figure, and the pings stand.
	#[test]
	fn a_file_has_none() {
		let file = std::fs::File::open(std::env::current_exe().unwrap()).unwrap();
		assert_eq!(smoothed(&file), None);
	}

	#[cfg(not(target_os = "linux"))]
	#[test]
	fn off_linux_there_is_none() {
		let (dialed, _accepted) = exchanged();
		assert_eq!(smoothed(&dialed), None);
	}
}
