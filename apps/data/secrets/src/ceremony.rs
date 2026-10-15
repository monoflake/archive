//! The lock set for the first time: `secrets init`, run once on a core by `mise run seal init`, in
//! this image with no network. It makes the root key, splits it, writes each core's share and the
//! key encrypted to the recovery age recipient into one directory, and nothing else; the task moves
//! each share to its core and the ciphertext to git. Nothing it prints holds a key. See RFC 231yq.
//!
//! The recovery recipient is post-quantum, `age1pq1`, which the `age` crate does not encrypt to, so
//! the key is piped to age's own program, pinned in the image at `/age`.

use crate::seal::{self, RootKey};
use std::fs::OpenOptions;
use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
use std::process::{Command, Stdio};

/// The recovery ciphertext's file name, in the directory written and in the repository.
pub const RECOVERY: &str = "recovery.age";

/// age's program in the image; elsewhere, the one on the path.
const AGE: &str = "/age";

/// The post-quantum hybrid recipient, ML-KEM-768 and X25519, the only kind the root key is
/// encrypted to: a plain X25519 key, `age1` alone, is refused, so one cannot be passed by mistake.
const RECIPIENT: &str = "age1pq1";

/// What an age ciphertext armored starts and ends with.
const ARMOR: (&str, &str) =
	("-----BEGIN AGE ENCRYPTED FILE-----", "-----END AGE ENCRYPTED FILE-----");

/// What `secrets init` was asked.
#[derive(Debug, PartialEq, Eq)]
pub struct Asked {
	pub recipient: String,
	pub nodes: Vec<String>,
	pub out: String,
}

/// `init --recipient <age1...> --nodes <a,b,c> --out <directory>`, every flag once.
pub fn parse(arguments: &[String]) -> Result<Asked, String> {
	let (mut recipient, mut nodes, mut out) = (None, None, None);
	let mut given = arguments.iter();
	while let Some(flag) = given.next() {
		let slot = match flag.as_str() {
			"--recipient" => &mut recipient,
			"--nodes" => &mut nodes,
			"--out" => &mut out,
			other => return Err(format!("`{other}` is not a flag of init")),
		};
		let value = given.next().ok_or_else(|| format!("{flag} takes a value"))?;
		if slot.replace(value.clone()).is_some() {
			return Err(format!("{flag} is given twice"));
		}
	}
	let usage = "init --recipient <age1...> --nodes <a,b,c> --out <directory>";
	let (Some(recipient), Some(nodes), Some(out)) = (recipient, nodes, out) else {
		return Err(usage.into());
	};
	let nodes: Vec<String> = nodes.split(',').map(str::to_owned).collect();
	Ok(Asked { recipient, nodes, out })
}

/// Three lowercase letters, a machine's name: a share file is named for the core an operator copies
/// it to, the one place a node is not its id. See spec/architecture/control.md, "Who reads it, and
/// with what".
fn name_is_valid(name: &str) -> bool {
	name.len() == 3 && name.bytes().all(|b| b.is_ascii_lowercase())
}

/// Writes `<node>.share` for each of the three nodes, in the order given, and `recovery.age`, into
/// `out`, each made new at mode 600, so a lock already set is never written over.
pub fn init(asked: &Asked) -> Result<Vec<String>, String> {
	if !asked.recipient.starts_with(RECIPIENT) || asked.recipient.contains(char::is_whitespace) {
		return Err("the recovery recipient is a post-quantum age key, age1pq1...".into());
	}
	if asked.nodes.len() != seal::SHARES {
		return Err(format!(
			"the lock is split among {} cores, and {} are named",
			seal::SHARES,
			asked.nodes.len()
		));
	}
	for (index, node) in asked.nodes.iter().enumerate() {
		if !name_is_valid(node) || asked.nodes[..index].contains(node) {
			return Err(format!("`{node}` is not a core's name, or is named twice"));
		}
	}
	let out = Path::new(&asked.out);
	let key = RootKey::generate();
	let shares = seal::split(&key);
	let recovery = encrypt(&asked.recipient, &key)?;
	let mut written = Vec::new();
	for (node, share) in asked.nodes.iter().zip(&shares) {
		let name = format!("{node}.share");
		create(&out.join(&name), share.to_json().as_bytes())?;
		written.push(name);
	}
	create(&out.join(RECOVERY), recovery.as_bytes())?;
	written.push(RECOVERY.into());
	Ok(written)
}

/// The key in hex, armored to `recipient` by age, given it on stdin so it is never an argument.
fn encrypt(recipient: &str, key: &RootKey) -> Result<String, String> {
	let program = if Path::new(AGE).exists() { AGE } else { "age" };
	let mut age = Command::new(program)
		.args(["--encrypt", "--armor", "--recipient", recipient])
		.stdin(Stdio::piped())
		.stdout(Stdio::piped())
		.spawn()
		.map_err(|error| format!("age could not be started: {error}"))?;
	let given =
		age.stdin.take().expect("stdin was piped").write_all(format!("{}\n", *key.to_hex()).as_bytes());
	let output = age.wait_with_output().map_err(|error| format!("age did not finish: {error}"))?;
	given.map_err(|error| format!("age was not given the key: {error}"))?;
	let armored = String::from_utf8(output.stdout).unwrap_or_default();
	let trimmed = armored.trim();
	if !output.status.success() || !trimmed.starts_with(ARMOR.0) || !trimmed.ends_with(ARMOR.1) {
		return Err("age did not encrypt the root key to the recipient".into());
	}
	Ok(armored)
}

fn create(path: &Path, content: &[u8]) -> Result<(), String> {
	let mut file = OpenOptions::new()
		.write(true)
		.create_new(true)
		.mode(0o600)
		.open(path)
		.map_err(|error| format!("{}: {error}", path.display()))?;
	file
		.write_all(content)
		.and_then(|()| file.sync_all())
		.map_err(|error| format!("{}: {error}", path.display()))
}

#[cfg(test)]
mod tests {
	use super::*;
	use crate::seal::Share;
	use std::os::unix::fs::PermissionsExt;

	/// A recovery identity made by `age-keygen` in `directory`, post-quantum as the author's is, and
	/// its recipient; None, said so, where age 1.3 or later is not on the path.
	fn recovery_identity(directory: &Path) -> Option<(std::path::PathBuf, String)> {
		let identity = directory.join("identity.txt");
		let made =
			Command::new("age-keygen").arg("-pq").arg("-o").arg(&identity).stderr(Stdio::null()).status();
		if !made.is_ok_and(|status| status.success()) {
			eprintln!("age-keygen -pq is not on the path; age 1.3 or later is what the ceremony runs");
			return None;
		}
		let recipient = Command::new("age-keygen").arg("-y").arg(&identity).output().unwrap();
		Some((identity, String::from_utf8(recipient.stdout).unwrap().trim().to_owned()))
	}

	fn arguments(text: &str) -> Vec<String> {
		text.split_whitespace().map(str::to_owned).collect()
	}

	fn scratch(name: &str) -> std::path::PathBuf {
		let directory =
			std::env::temp_dir().join(format!("secrets-ceremony-{}-{name}", std::process::id()));
		let _ = std::fs::remove_dir_all(&directory);
		std::fs::create_dir_all(&directory).unwrap();
		directory
	}

	#[test]
	fn reads_its_flags_in_any_order() {
		let asked = parse(&arguments("--out /out --nodes tyo,sin,rdu --recipient age1x")).unwrap();
		assert_eq!(
			asked,
			Asked { recipient: "age1x".into(), nodes: arguments("tyo sin rdu"), out: "/out".into() }
		);
		for wrong in
			["", "--out /out", "--out /out --out /x --nodes a --recipient r", "--nodes", "--force x"]
		{
			assert!(parse(&arguments(wrong)).is_err(), "{wrong}");
		}
	}

	#[test]
	fn any_two_shares_written_and_the_recovery_ciphertext_open_one_key() {
		let keys = scratch("whole-keys");
		let Some((identity, recipient)) = recovery_identity(&keys) else { return };
		assert!(recipient.starts_with("age1pq1"), "{recipient}");
		let directory = scratch("whole");
		let asked =
			Asked { recipient, nodes: arguments("tyo sin rdu"), out: directory.display().to_string() };
		let written = init(&asked).unwrap();
		assert_eq!(written, ["tyo.share", "sin.share", "rdu.share", "recovery.age"]);
		for name in &written {
			let mode = std::fs::metadata(directory.join(name)).unwrap().permissions().mode();
			assert_eq!(mode & 0o777, 0o600, "{name}");
		}
		let read = |node: &str| {
			Share::from_json(&std::fs::read_to_string(directory.join(format!("{node}.share"))).unwrap())
				.unwrap()
		};
		let shares = [read("tyo"), read("sin"), read("rdu")];
		assert_eq!(shares.iter().map(Share::identifier).collect::<Vec<_>>(), [1, 2, 3]);
		let key = seal::rebuild(&shares[1..]).unwrap();
		assert_eq!(seal::rebuild(&shares[..2]).unwrap().bytes(), key.bytes());

		let armored = std::fs::read(directory.join(RECOVERY)).unwrap();
		assert!(armored.starts_with(ARMOR.0.as_bytes()));
		let opened = Command::new("age")
			.arg("--decrypt")
			.arg("--identity")
			.arg(&identity)
			.arg(directory.join(RECOVERY))
			.output()
			.unwrap();
		assert!(opened.status.success());
		let opened = RootKey::from_hex(std::str::from_utf8(&opened.stdout).unwrap()).unwrap();
		assert_eq!(opened.bytes(), key.bytes());

		let again = init(&asked);
		assert!(again.unwrap_err().contains("tyo.share"), "a lock already set was written over");
		assert_eq!(read("tyo").identifier(), 1);
		std::fs::remove_dir_all(directory).unwrap();
		std::fs::remove_dir_all(keys).unwrap();
	}

	#[test]
	fn refuses_what_it_cannot_split_for() {
		// Refused before age is asked, so no age is needed.
		let recipient = format!("age1pq1{}", "q".repeat(1952));
		let directory = scratch("refused");
		let out = directory.display().to_string();
		for (recipient, nodes) in [
			("ssh-ed25519 AAAA".to_owned(), "tyo sin rdu"),
			// A plain X25519 key, of the shape the sops key has.
			(format!("age1{}", "q".repeat(58)), "tyo sin rdu"),
			(format!("{recipient} {recipient}"), "tyo sin rdu"),
			(recipient.clone(), "tyo sin"),
			(recipient.clone(), "tyo sin rdu buf"),
			(recipient.clone(), "tyo tyo rdu"),
			(recipient, "tyo sin RDU"),
		] {
			let asked = Asked { recipient, nodes: arguments(nodes), out: out.clone() };
			assert!(init(&asked).is_err(), "{nodes}");
		}
		assert_eq!(std::fs::read_dir(&directory).unwrap().count(), 0, "a refusal wrote something");
		std::fs::remove_dir_all(directory).unwrap();
	}
}
