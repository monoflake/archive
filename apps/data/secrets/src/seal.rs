//! The root key, split two of three by Shamir's scheme among the cores, and the lock it is held
//! behind: sealed until two shares or the operator's whole key rebuild it, and then only in memory.
//! See RFC 231yq.

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::sync::RwLock;
use vsss_rs::Gf256;
use zeroize::{Zeroize, ZeroizeOnDrop, Zeroizing};

/// The root key's length, and every data key's.
pub const KEY_BYTES: usize = 32;
/// Any two shares rebuild the key, and there are three, one a core.
pub const THRESHOLD: usize = 2;
pub const SHARES: usize = 3;

/// Put in front of the key before it is hashed into its check, so the check is of nothing else.
const CHECK_LABEL: &[u8] = b"monoflake secrets root key check v1\0";

/// The key every data key is wrapped by. It is never written anywhere but the recovery ciphertext,
/// and is zeroed when dropped.
#[derive(Zeroize, ZeroizeOnDrop)]
pub struct RootKey([u8; KEY_BYTES]);

impl RootKey {
	/// A new key from the system's randomness.
	pub fn generate() -> Self {
		let mut key = [0; KEY_BYTES];
		getrandom::fill(&mut key).expect("the system has no randomness to give");
		Self(key)
	}

	pub fn from_bytes(bytes: &[u8]) -> Option<Self> {
		<[u8; KEY_BYTES]>::try_from(bytes).ok().map(Self)
	}

	/// The key in lowercase hex, as the operator gives it back to unseal a lone core.
	pub fn from_hex(text: &str) -> Option<Self> {
		let bytes = Zeroizing::new(hex::decode(text.trim()).ok()?);
		Self::from_bytes(&bytes)
	}

	pub fn to_hex(&self) -> Zeroizing<String> {
		Zeroizing::new(hex::encode(self.0))
	}

	pub fn bytes(&self) -> &[u8; KEY_BYTES] {
		&self.0
	}

	/// A digest that says whether a key is this one and nothing more about it, kept beside every
	/// share so a key rebuilt from the wrong shares, or given wrong, is refused rather than used.
	pub fn check(&self) -> [u8; 32] {
		let mut hash = Sha256::new();
		hash.update(CHECK_LABEL);
		hash.update(self.0);
		hash.finalize().into()
	}
}

/// One core's share of the root key, and the check of the key it rebuilds. The bytes are vsss-rs's:
/// the share's identifier, 1 to 3, then a byte for each of the key's.
#[derive(Clone, PartialEq, Eq)]
pub struct Share {
	bytes: Zeroizing<Vec<u8>>,
	check: [u8; 32],
}

/// A share as its file holds it and as a peer is answered it.
#[derive(Serialize, Deserialize)]
pub struct Written {
	pub version: u32,
	pub share: String,
	pub check: String,
}

impl Drop for Written {
	fn drop(&mut self) {
		self.share.zeroize();
	}
}

impl Share {
	/// Which share of the three this is.
	pub fn identifier(&self) -> u8 {
		self.bytes[0]
	}

	pub fn check(&self) -> [u8; 32] {
		self.check
	}

	pub fn written(&self) -> Written {
		Written {
			version: 1,
			share: STANDARD.encode(self.bytes.as_slice()),
			check: hex::encode(self.check),
		}
	}

	/// The share a file or a peer holds, or why it is not one.
	pub fn from_written(written: &Written) -> Result<Self, &'static str> {
		if written.version != 1 {
			return Err("a share of a version this service does not read");
		}
		let bytes =
			Zeroizing::new(STANDARD.decode(written.share.as_bytes()).map_err(|_| "a share is base64")?);
		if bytes.len() != KEY_BYTES + 1 || bytes[0] == 0 {
			return Err("a share is its identifier and a byte for each of the key's");
		}
		let check = hex::decode(&written.check)
			.ok()
			.and_then(|check| <[u8; 32]>::try_from(check).ok())
			.ok_or("a share's check is 32 bytes of hex")?;
		Ok(Self { bytes, check })
	}

	pub fn from_json(text: &str) -> Result<Self, &'static str> {
		let written: Written = serde_json::from_str(text).map_err(|_| "a share is a JSON object")?;
		Self::from_written(&written)
	}

	pub fn to_json(&self) -> Zeroizing<String> {
		Zeroizing::new(serde_json::to_string(&self.written()).expect("a share always serializes"))
	}
}

/// The key split into three shares, any two of which rebuild it.
pub fn split(key: &RootKey) -> Vec<Share> {
	let rng = getrandom::rand_core::UnwrapErr(getrandom::SysRng);
	let check = key.check();
	Gf256::split_array(THRESHOLD, SHARES, key.bytes(), rng)
		.expect("two of three is a threshold vsss-rs splits")
		.into_iter()
		.map(|bytes| Share { bytes: Zeroizing::new(bytes), check })
		.collect()
}

/// The key two of `shares` rebuild, checked against the check they carry: every pair of distinct
/// shares agreeing on one check is tried, so one share that is wrong does not stop two that are
/// right. None when no pair rebuilds the key its check names.
pub fn rebuild(shares: &[Share]) -> Option<RootKey> {
	for (index, first) in shares.iter().enumerate() {
		for second in &shares[index + 1..] {
			if first.identifier() == second.identifier() || first.check != second.check {
				continue;
			}
			let pair = [first.bytes.to_vec(), second.bytes.to_vec()];
			let combined = Gf256::combine_array(&pair);
			for mut bytes in pair {
				bytes.zeroize();
			}
			let Ok(combined) = combined.map(Zeroizing::new) else { continue };
			if let Some(key) = RootKey::from_bytes(&combined)
				&& key.check() == first.check
			{
				return Some(key);
			}
		}
	}
	None
}

/// Whether the lock is open, and this core's share, which it holds for its peers either way.
#[derive(Default)]
pub struct Seal {
	key: RwLock<Option<RootKey>>,
	share: RwLock<Option<Share>>,
}

impl Seal {
	pub fn is_sealed(&self) -> bool {
		self.key.read().expect("the seal's lock is never poisoned").is_none()
	}

	/// Opens the lock with `key`, unless it is already open. Whoever opens it has already checked
	/// the key.
	pub fn open(&self, key: RootKey) {
		let mut held = self.key.write().expect("the seal's lock is never poisoned");
		if held.is_none() {
			*held = Some(key);
		}
	}

	/// What `with` answers of the root key, or None while sealed. The key never leaves the lock.
	pub fn with_key<T>(&self, with: impl FnOnce(&RootKey) -> T) -> Option<T> {
		self.key.read().expect("the seal's lock is never poisoned").as_ref().map(with)
	}

	pub fn share(&self) -> Option<Share> {
		self.share.read().expect("the seal's lock is never poisoned").clone()
	}

	pub fn hold(&self, share: Share) {
		*self.share.write().expect("the seal's lock is never poisoned") = Some(share);
	}
}

#[cfg(test)]
mod tests {
	use super::*;

	#[test]
	fn any_two_of_three_rebuild_the_key() {
		let key = RootKey::generate();
		let shares = split(&key);
		assert_eq!(shares.len(), 3);
		for (a, b) in [(0, 1), (0, 2), (1, 2), (2, 0)] {
			let rebuilt = rebuild(&[shares[a].clone(), shares[b].clone()]).unwrap();
			assert_eq!(rebuilt.bytes(), key.bytes(), "{a} and {b}");
		}
		assert_eq!(rebuild(&shares).unwrap().bytes(), key.bytes());
	}

	#[test]
	fn one_share_alone_rebuilds_nothing() {
		let shares = split(&RootKey::generate());
		for share in &shares {
			assert!(rebuild(std::slice::from_ref(share)).is_none());
			assert!(rebuild(&[share.clone(), share.clone()]).is_none());
		}
		assert!(rebuild(&[]).is_none());
	}

	#[test]
	fn a_share_of_another_key_is_passed_over() {
		let key = RootKey::generate();
		let shares = split(&key);
		let other = split(&RootKey::generate());
		assert!(rebuild(&[shares[0].clone(), other[1].clone()]).is_none());
		let mixed = [other[1].clone(), shares[0].clone(), shares[2].clone()];
		assert_eq!(rebuild(&mixed).unwrap().bytes(), key.bytes());
	}

	#[test]
	fn a_share_altered_under_its_check_is_refused() {
		let key = RootKey::generate();
		let shares = split(&key);
		let mut altered = shares[1].clone();
		altered.bytes[5] ^= 1;
		assert!(rebuild(&[shares[0].clone(), altered.clone()]).is_none());
		assert_eq!(
			rebuild(&[shares[0].clone(), altered, shares[2].clone()]).unwrap().bytes(),
			key.bytes()
		);
	}

	#[test]
	fn a_share_survives_its_file() {
		let shares = split(&RootKey::generate());
		let read = Share::from_json(&shares[2].to_json()).unwrap();
		assert!(read == shares[2]);
		assert_eq!(read.identifier(), 3);
	}

	#[test]
	fn a_file_that_is_not_a_share_is_refused() {
		let share = split(&RootKey::generate()).remove(0);
		let good = share.written();
		let mut cases = vec![
			Written { version: 2, share: good.share.clone(), check: good.check.clone() },
			Written { version: 1, share: "not base64!".into(), check: good.check.clone() },
			Written { version: 1, share: STANDARD.encode([1; 32]), check: good.check.clone() },
			Written { version: 1, share: STANDARD.encode([0; 33]), check: good.check.clone() },
			Written { version: 1, share: good.share.clone(), check: "00".into() },
		];
		for written in cases.drain(..) {
			assert!(Share::from_written(&written).is_err());
		}
		assert!(Share::from_json("[]").is_err());
	}

	#[test]
	fn the_key_reads_back_from_its_hex() {
		let key = RootKey::generate();
		assert_eq!(RootKey::from_hex(&format!(" {}\n", *key.to_hex())).unwrap().bytes(), key.bytes());
		assert!(RootKey::from_hex("abcd").is_none());
		assert!(RootKey::from_hex(&"zz".repeat(32)).is_none());
	}

	#[test]
	fn the_seal_opens_once() {
		let seal = Seal::default();
		assert!(seal.is_sealed());
		assert!(seal.with_key(|key| key.check()).is_none());
		let key = RootKey::generate();
		let check = key.check();
		seal.open(key);
		seal.open(RootKey::generate());
		assert!(!seal.is_sealed());
		assert_eq!(seal.with_key(RootKey::check), Some(check));
	}
}
