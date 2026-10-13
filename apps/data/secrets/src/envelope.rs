//! Envelope encryption: each value under a data key of its own, the data key under the root key,
//! both with XChaCha20-Poly1305, and both bound to the secret's identity so ciphertext moved to
//! another row does not open. See RFC 231yq.

use crate::seal::{KEY_BYTES, RootKey};
use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{Key, XChaCha20Poly1305, XNonce};
use zeroize::Zeroizing;

/// XChaCha20's nonce, long enough to be drawn at random for every message.
const NONCE_BYTES: usize = 24;

/// Put in front of every associated data, so none is ever taken for another protocol's.
const LABEL: &[u8] = b"monoflake secrets v1";

/// What a secret is: its scope, its app and its name.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Identity<'a> {
	pub scope: &'a str,
	pub app: &'a str,
	pub name: &'a str,
}

/// A value as it is stored: its data key wrapped by the root key, and the value under its data key,
/// each a nonce followed by the ciphertext and its tag.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Sealed {
	pub data_key: Vec<u8>,
	pub value: Vec<u8>,
}

/// A row that does not open under this key as this identity: moved, altered, or of another key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Unopened;

impl std::fmt::Display for Unopened {
	fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
		formatter.write_str("a stored secret did not open under the root key as its own identity")
	}
}

impl std::error::Error for Unopened {}

/// The associated data of one `part` of a secret, each field prefixed by its length so no two
/// identities can spell the same bytes.
fn associated(identity: &Identity, part: &str) -> Vec<u8> {
	let mut data = LABEL.to_vec();
	for field in [part, identity.scope, identity.app, identity.name] {
		let length = u32::try_from(field.len()).expect("a field is shorter than 4 GiB");
		data.extend_from_slice(&length.to_be_bytes());
		data.extend_from_slice(field.as_bytes());
	}
	data
}

fn encrypt(key: &[u8; KEY_BYTES], associated: &[u8], plaintext: &[u8]) -> Vec<u8> {
	let cipher = XChaCha20Poly1305::new(&Key::from(*key));
	let mut nonce = [0; NONCE_BYTES];
	getrandom::fill(&mut nonce).expect("the system has no randomness to give");
	let sealed = cipher
		.encrypt(&XNonce::from(nonce), Payload { msg: plaintext, aad: associated })
		.expect("XChaCha20-Poly1305 encrypts anything shorter than 256 GiB");
	[nonce.as_slice(), &sealed].concat()
}

fn decrypt(
	key: &[u8; KEY_BYTES],
	associated: &[u8],
	sealed: &[u8],
) -> Result<Zeroizing<Vec<u8>>, Unopened> {
	let (nonce, ciphertext) = sealed.split_at_checked(NONCE_BYTES).ok_or(Unopened)?;
	let nonce = <[u8; NONCE_BYTES]>::try_from(nonce).map_err(|_| Unopened)?;
	let cipher = XChaCha20Poly1305::new(&Key::from(*key));
	cipher
		.decrypt(&XNonce::from(nonce), Payload { msg: ciphertext, aad: associated })
		.map(Zeroizing::new)
		.map_err(|_| Unopened)
}

/// `value` under a new data key, and that key under `root`, both bound to `identity`.
pub fn seal(root: &RootKey, identity: &Identity, value: &[u8]) -> Sealed {
	let mut data_key = Zeroizing::new([0; KEY_BYTES]);
	getrandom::fill(data_key.as_mut()).expect("the system has no randomness to give");
	Sealed {
		data_key: encrypt(root.bytes(), &associated(identity, "key"), data_key.as_ref()),
		value: encrypt(&data_key, &associated(identity, "value"), value),
	}
}

/// The value `sealed` holds, if it opens under `root` as `identity`.
pub fn open(
	root: &RootKey,
	identity: &Identity,
	sealed: &Sealed,
) -> Result<Zeroizing<Vec<u8>>, Unopened> {
	let unwrapped = decrypt(root.bytes(), &associated(identity, "key"), &sealed.data_key)?;
	let data_key =
		Zeroizing::new(<[u8; KEY_BYTES]>::try_from(unwrapped.as_slice()).map_err(|_| Unopened)?);
	decrypt(&data_key, &associated(identity, "value"), &sealed.value)
}

#[cfg(test)]
mod tests {
	use super::*;

	const ID: Identity = Identity { scope: "platform", app: "gemini", name: "GEMINI_KEY" };

	#[test]
	fn a_value_opens_as_it_was_sealed() {
		let root = RootKey::generate();
		let sealed = seal(&root, &ID, b"s3cret value");
		assert_eq!(open(&root, &ID, &sealed).unwrap().as_slice(), b"s3cret value");
		assert_eq!(open(&root, &ID, &seal(&root, &ID, b"")).unwrap().as_slice(), b"");
	}

	#[test]
	fn the_same_value_seals_differently_each_time() {
		let root = RootKey::generate();
		let (one, two) = (seal(&root, &ID, b"same"), seal(&root, &ID, b"same"));
		assert_ne!(one.value, two.value);
		assert_ne!(one.data_key, two.data_key);
	}

	#[test]
	fn a_row_moved_to_another_identity_does_not_open() {
		let root = RootKey::generate();
		let sealed = seal(&root, &ID, b"s3cret");
		for moved in [
			Identity { scope: "canmi", ..ID },
			Identity { app: "grok", ..ID },
			Identity { name: "GEMINI_KEY_2", ..ID },
			// The same bytes run together differently: the lengths keep them apart.
			Identity { scope: "platformg", app: "emini", ..ID },
		] {
			assert_eq!(open(&root, &moved, &sealed), Err(Unopened), "{moved:?}");
		}
	}

	#[test]
	fn a_data_key_swapped_between_rows_does_not_open() {
		let root = RootKey::generate();
		let other = Identity { name: "OTHER", ..ID };
		let (mine, theirs) = (seal(&root, &ID, b"mine"), seal(&root, &other, b"theirs"));
		let swapped = Sealed { data_key: theirs.data_key, value: mine.value };
		assert_eq!(open(&root, &ID, &swapped), Err(Unopened));
		assert_eq!(open(&root, &other, &swapped), Err(Unopened));
	}

	#[test]
	fn another_root_key_or_an_altered_byte_does_not_open() {
		let root = RootKey::generate();
		let sealed = seal(&root, &ID, b"s3cret");
		assert_eq!(open(&RootKey::generate(), &ID, &sealed), Err(Unopened));
		for at in [0, 30, sealed.value.len() - 1] {
			let mut altered = sealed.clone();
			altered.value[at] ^= 1;
			assert_eq!(open(&root, &ID, &altered), Err(Unopened), "value byte {at}");
		}
		let mut altered = sealed.clone();
		altered.data_key[40] ^= 1;
		assert_eq!(open(&root, &ID, &altered), Err(Unopened));
		let truncated = Sealed { data_key: sealed.data_key[..10].to_vec(), value: sealed.value };
		assert_eq!(open(&root, &ID, &truncated), Err(Unopened));
	}
}
