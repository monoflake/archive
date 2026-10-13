"""seal's ceremony, with every machine faked: the shell it runs on a core is run here against fake
docker, ssh, chown, mountpoint and shred, and the task around it with SSH mocked:
`python3 .mise/tasks/seal_test.py`."""

import contextlib
import hashlib
import importlib.machinery
import importlib.util
import io
import os
import stat
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
loader = importlib.machinery.SourceFileLoader("task_seal", str(HERE / "seal"))
seal = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
loader.exec_module(seal)

RECIPIENT = "age1pq1" + "q" * 1952
X25519 = "age1" + "q" * 58
CIPHERTEXT = f"{seal.ARMOR[0]}\nYWdlLWVuY3J5cHRpb24ub3JnL3Yx\n{seal.ARMOR[1]}\n"

# Each fake records what it was asked in $LOG; ssh keeps each share it was sent in $SENT/<address>,
# and fails a send, or a reach, to the address named in $FAIL_SEND or $FAIL_REACH.
FAKES = {
	"docker": r"""#!/bin/sh
echo "docker $*" >> "$LOG"
if [ "$1" = inspect ]; then echo ghcr.io/monoflake/platform/secrets:test; exit 0; fi
out= nodes=
while [ $# -gt 0 ]; do
	case $1 in
		-v) out=${2%%:*}; shift ;;
		--nodes) nodes=$2; shift ;;
	esac
	shift
done
for node in $(echo "$nodes" | tr , ' '); do printf 'share-of-%s' "$node" > "$out/$node.share"; done
printf '%s' "$CIPHERTEXT" > "$out/recovery.age"
""",
	"ssh": r"""#!/bin/sh
echo "ssh $*" >> "$LOG"
for last; do :; done
host=
for arg; do case $arg in root@*) host=${arg#root@} ;; esac; done
if [ "$last" = true ]; then [ "$host" != "$FAIL_REACH" ]; exit; fi
[ "$host" != "$FAIL_SEND" ] || exit 1
printf '%s' "$last" > "$SENT/$host.command"
cat > "$SENT/$host"
""",
	"chown": '#!/bin/sh\necho "chown $*" >> "$LOG"\n',
	"mountpoint": "#!/bin/sh\nexit 0\n",
	"shred": '#!/bin/sh\necho "shred $*" >> "$LOG"\n[ "$1" = -u ] && rm -f "$2"\n',
}


class Shell(unittest.TestCase):
	"""INIT and INSTALL run by sh, here, with every command that would reach a machine faked."""

	def setUp(self):
		self.scratch = tempfile.TemporaryDirectory()
		self.root = Path(self.scratch.name)
		bin = self.root / "bin"
		bin.mkdir()
		for name, text in FAKES.items():
			(bin / name).write_text(text)
			(bin / name).chmod(0o755)
		(self.root / "sent").mkdir()
		self.seal = self.root / "data" / "seal"
		self.seal.parent.mkdir()
		self.env = {
			**os.environ,
			"PATH": f"{bin}:{os.environ['PATH']}",
			"LOG": str(self.root / "log"),
			"SENT": str(self.root / "sent"),
			"CIPHERTEXT": CIPHERTEXT,
		}

	def tearDown(self):
		self.scratch.cleanup()

	def run_init(self, **env):
		arguments = [
			str(self.seal), seal.USER, RECIPIENT, "tyo", seal.INSTALL, seal.install_command(),
			"sin=100.64.0.2", "rdu=100.64.0.3",
		]
		return subprocess.run(
			["sh", "-c", seal.INIT, "sh", *arguments],
			env={**self.env, **env},
			text=True,
			capture_output=True,
		)

	def log(self):
		path = self.root / "log"
		return path.read_text() if path.exists() else ""

	def leftovers(self):
		return [p.name for p in self.seal.parent.iterdir() if p.name.startswith("seal.init.")]

	def test_each_core_is_given_its_own_share_and_the_operator_the_ciphertext_alone(self):
		done = self.run_init()
		self.assertEqual(done.returncode, 0, done.stderr)
		self.assertEqual(done.stdout, CIPHERTEXT)
		self.assertNotIn("share-of", done.stdout + done.stderr)
		share = self.seal / "share"
		self.assertEqual(share.read_text(), "share-of-tyo")
		self.assertEqual(stat.S_IMODE(share.stat().st_mode), 0o400)
		self.assertEqual(stat.S_IMODE(self.seal.stat().st_mode), 0o700)
		sent = self.root / "sent"
		self.assertEqual((sent / "100.64.0.2").read_text(), "share-of-sin")
		self.assertEqual((sent / "100.64.0.3").read_text(), "share-of-rdu")
		self.assertEqual((sent / "100.64.0.2.command").read_text(), seal.install_command())
		self.assertIn("--network none", self.log())
		self.assertIn("--nodes tyo,sin,rdu", self.log())
		self.assertIn(f"chown {seal.USER} {self.seal} {self.seal}/share.new", self.log())
		self.assertEqual(self.leftovers(), [], "the working directory outlived the ceremony")

	def test_nothing_is_made_when_a_core_is_not_reached(self):
		done = self.run_init(FAIL_REACH="100.64.0.3")
		self.assertNotEqual(done.returncode, 0)
		self.assertIn("rdu is not reached", done.stderr)
		self.assertNotIn("docker run", self.log())
		self.assertEqual(done.stdout, "")
		self.assertFalse((self.seal / "share").exists())

	def test_a_failed_send_still_hands_back_the_ciphertext_and_leaves_nothing_behind(self):
		done = self.run_init(FAIL_SEND="100.64.0.3")
		self.assertNotEqual(done.returncode, 0)
		self.assertEqual(done.stdout, CIPHERTEXT)
		self.assertIn("rdu: writing its share failed", done.stderr)
		self.assertTrue((self.seal / "share").exists())
		self.assertEqual(self.leftovers(), [])

	def shell(self, script, *arguments):
		return subprocess.run(
			["sh", "-c", script, "sh", *arguments], env=self.env, text=True, capture_output=True
		)

	def test_prepare_makes_an_empty_seal_and_leaves_a_share_as_it_is(self):
		for _ in range(2):
			made = self.shell(seal.PREPARE, str(self.seal), seal.USER)
			self.assertEqual((made.returncode, made.stdout.strip()), (0, "empty"), made.stderr)
		self.assertEqual(stat.S_IMODE(self.seal.stat().st_mode), 0o700)
		self.assertIn(f"chown {seal.USER} {self.seal}\n", self.log())
		share = self.seal / "share"
		share.write_text("share-of-tyo")
		share.chmod(0o400)
		again = self.shell(seal.PREPARE, str(self.seal), seal.USER)
		self.assertEqual((again.returncode, again.stdout.strip()), (0, "held"))
		self.assertEqual(share.read_text(), "share-of-tyo")
		self.assertEqual(stat.S_IMODE(share.stat().st_mode), 0o400)
		self.assertNotIn(str(share), self.log(), "prepare touched the share")

	def test_an_empty_seal_is_absent_to_every_check_and_init_proceeds(self):
		self.assertEqual(self.shell(seal.PREPARE, str(self.seal), seal.USER).returncode, 0)
		self.assertEqual(self.shell(seal.HELD, str(self.seal)).stdout.strip(), "absent")
		done = self.run_init()
		self.assertEqual(done.returncode, 0, done.stderr)
		self.assertEqual((self.seal / "share").read_text(), "share-of-tyo")
		self.assertEqual(self.shell(seal.HELD, str(self.seal)).stdout.strip(), "held")

	def test_a_share_is_never_written_over_and_never_empty(self):
		def install(stdin):
			return subprocess.run(
				["sh", "-c", seal.INSTALL, "sh", str(self.seal), seal.USER],
				input=stdin, env=self.env, text=True, capture_output=True,
			)

		self.assertNotEqual(install("").returncode, 0)
		self.assertFalse(self.seal.joinpath("share.new").exists())
		self.assertEqual(install("first").returncode, 0)
		second = install("second")
		self.assertNotEqual(second.returncode, 0)
		self.assertIn("already exists", second.stderr)
		self.assertEqual((self.seal / "share").read_text(), "first")

	def test_a_lock_already_set_on_the_first_core_stops_the_ceremony(self):
		self.seal.mkdir()
		(self.seal / "share").write_text("old")
		done = self.run_init()
		self.assertNotEqual(done.returncode, 0)
		self.assertEqual((self.seal / "share").read_text(), "old")
		self.assertEqual(done.stdout, "")
		self.assertNotIn("docker run", self.log())


class Task(unittest.TestCase):
	"""The task around the shell, its every reach to a machine mocked."""

	def setUp(self):
		self.scratch = tempfile.TemporaryDirectory()
		recovery = Path(self.scratch.name) / "recovery.age"
		self.public = Path(self.scratch.name) / "recovery.pub"
		patches = [
			mock.patch.object(seal, "RECOVERY", recovery),
			mock.patch.object(seal, "PUBLIC", self.public),
			mock.patch.object(seal.database, "ROOT", Path(self.scratch.name)),
			mock.patch.object(seal, "cores", lambda: ["tyo", "sin", "rdu"]),
		]
		for patch in patches:
			patch.start()
			self.addCleanup(patch.stop)
		self.recovery = recovery

	def tearDown(self):
		self.scratch.cleanup()

	def quietly(self, verb, *arguments, dry=False):
		out = io.StringIO()
		with contextlib.redirect_stdout(out):
			verb(list(arguments), dry)
		return out.getvalue()

	def test_a_recipient_that_is_not_post_quantum_is_refused(self):
		wrong = [
			[],
			["--recipient", X25519],
			["--recipient", "age1pq1short"],
			["--recipient", RECIPIENT.upper()],
			["--recipient"],
			[RECIPIENT],
			["--recipient", RECIPIENT, "--recipient", RECIPIENT],
			["--recipient", "ssh-ed25519 AAAA"],
		]
		for arguments in wrong:
			with self.assertRaises(SystemExit), mock.patch.object(seal, "held") as asked:
				self.quietly(seal.init, *arguments)
			asked.assert_not_called()

	def test_an_x25519_key_in_recovery_pub_is_refused_too(self):
		self.public.write_text(f"# The recovery key's recipient.\n{X25519}\n")
		self.assertEqual(seal.published(), X25519)
		with self.assertRaises(SystemExit), mock.patch.object(seal, "held") as asked:
			self.quietly(seal.init, dry=True)
		asked.assert_not_called()

	def test_the_recipient_is_read_from_recovery_pub_when_none_is_given(self):
		self.public.write_text(f"# The recovery key's recipient.\n# Offline.\n{RECIPIENT}\n")
		self.assertEqual(seal.published(), RECIPIENT)
		said = self.quietly(seal.init, dry=True)
		self.assertIn("age1pq1qqqqqqqqq...", said)
		self.assertNotIn(RECIPIENT, said)
		self.public.write_text(f"{RECIPIENT}\n{RECIPIENT}\n")
		self.assertIsNone(seal.published())
		with self.assertRaises(SystemExit):
			self.quietly(seal.init, dry=True)

	def test_the_committed_recovery_pub_is_accepted(self):
		committed = HERE.parents[1] / "recovery.pub"
		with mock.patch.object(seal, "PUBLIC", committed):
			self.assertIsNotNone(seal.recipient_of([]))

	def test_a_dry_run_reaches_no_machine(self):
		explode = mock.Mock(side_effect=AssertionError("reached a machine"))
		with mock.patch.object(seal.database, "ssh", explode), mock.patch.object(seal.subprocess, "run", explode):
			said = self.quietly(seal.init, "--recipient", RECIPIENT, dry=True)
		self.assertIn("would write its share", said)
		self.assertFalse(self.recovery.exists())

	def test_a_lock_already_set_is_refused_before_anything_is_made(self):
		self.recovery.write_text(CIPHERTEXT)
		with self.assertRaises(SystemExit), mock.patch.object(seal, "forwarded") as made:
			self.quietly(seal.init, "--recipient", RECIPIENT)
		made.assert_not_called()
		self.recovery.unlink()
		with (
			mock.patch.object(seal, "held", lambda node: node == "rdu"),
			mock.patch.object(seal, "forwarded") as made,
			self.assertRaises(SystemExit),
		):
			self.quietly(seal.init, "--recipient", RECIPIENT)
		made.assert_not_called()
		with (
			mock.patch.object(seal, "held", lambda node: None),
			mock.patch.object(seal, "forwarded") as made,
			self.assertRaises(SystemExit),
		):
			self.quietly(seal.init, "--recipient", RECIPIENT)
		made.assert_not_called()

	def test_the_first_core_sets_the_lock_and_the_ciphertext_is_written_here(self):
		addresses = {"sin": "100.64.0.2", "rdu": "100.64.0.3"}
		with (
			mock.patch.object(seal, "held", lambda node: False),
			mock.patch.object(seal.database, "tailnet_ipv4", addresses.__getitem__),
			mock.patch.object(seal, "forwarded", return_value=(True, CIPHERTEXT)) as made,
		):
			said = self.quietly(seal.init, "--recipient", RECIPIENT)
		node, command = made.call_args.args
		self.assertEqual(node, "tyo")
		self.assertIn("sin=100.64.0.2", command)
		self.assertIn("rdu=100.64.0.3", command)
		self.assertIn(RECIPIENT, command)
		self.assertEqual(self.recovery.read_text(), CIPHERTEXT)
		self.assertIn("commit recovery.age", said)

	def test_a_failure_partway_keeps_the_ciphertext_and_a_wrong_answer_keeps_nothing(self):
		with (
			mock.patch.object(seal, "held", lambda node: False),
			mock.patch.object(seal.database, "tailnet_ipv4", lambda node: "100.64.0.9"),
			mock.patch.object(seal, "forwarded", return_value=(False, CIPHERTEXT)),
			self.assertRaises(SystemExit),
		):
			self.quietly(seal.init, "--recipient", RECIPIENT)
		self.assertEqual(self.recovery.read_text(), CIPHERTEXT)
		self.recovery.unlink()
		for answer in [(True, "share-of-tyo"), (False, ""), (True, CIPHERTEXT + "more")]:
			with (
				mock.patch.object(seal, "held", lambda node: False),
				mock.patch.object(seal.database, "tailnet_ipv4", lambda node: "100.64.0.9"),
				mock.patch.object(seal, "forwarded", return_value=answer),
				self.assertRaises(SystemExit),
			):
				self.quietly(seal.init, "--recipient", RECIPIENT)
			self.assertFalse(self.recovery.exists(), answer)

	def test_prepare_reaches_every_core_and_refuses_any_other_node(self):
		with mock.patch.object(seal.database, "ssh", return_value="empty") as reached:
			said = self.quietly(seal.prepare)
		self.assertEqual([call.args[0] for call in reached.call_args_list], ["tyo", "sin", "rdu"])
		self.assertTrue(all(seal.shlex.quote(seal.PREPARE) in call.args[1] for call in reached.call_args_list))
		self.assertIn("tyo: /data/seal is ready, empty", said)
		with mock.patch.object(seal.database, "ssh", return_value="held") as reached:
			said = self.quietly(seal.prepare, "sin")
		self.assertEqual([call.args[0] for call in reached.call_args_list], ["sin"])
		self.assertIn("holding its share", said)
		for arguments in [["buf"], ["tyo", "buf"]]:
			with mock.patch.object(seal.database, "ssh") as reached, self.assertRaises(SystemExit):
				self.quietly(seal.prepare, *arguments)
			reached.assert_not_called()
		with mock.patch.object(seal.database, "ssh", return_value=None), self.assertRaises(SystemExit):
			self.quietly(seal.prepare)
		explode = mock.Mock(side_effect=AssertionError("reached a machine"))
		with mock.patch.object(seal.database, "ssh", explode):
			self.assertIn("would make /data/seal", self.quietly(seal.prepare, dry=True))

	def test_unseal_checks_before_it_opens_anything(self):
		explode = mock.Mock(side_effect=AssertionError("reached a machine"))
		with mock.patch.object(seal.subprocess, "run", explode):
			for arguments in [["tyo"], ["TYO", "key.txt"], ["tyo", "key.txt"]]:
				with self.assertRaises(SystemExit):
					self.quietly(seal.unseal, *arguments)
			self.recovery.write_text(CIPHERTEXT)
			said = self.quietly(seal.unseal, "tyo", "key.txt", dry=True)
		self.assertIn("would send the root key", said)


# One host token, the read token derived from it and that token's digest, pinned here and in the
# service's src/tokens.rs, so the task and the service cannot drift apart.
HOST = "host-s3cret"
READ = "e427d2347bcf34900e081847095ac6d30609ba546b08e680d5f8f4ee8c389116"
DIGEST = "d0cd53f06568c9cdf0be664afcf1aa86acef7179e1840e1377afe84c902ba564"


class Token(unittest.TestCase):
	"""`seal token`, with secrets.json, infra's tokens, the tailnet and every core faked."""

	def setUp(self):
		self.scratch = tempfile.TemporaryDirectory()
		nodes = Path(self.scratch.name) / "nodes.toml"
		nodes.write_text("[tyo]\n[sin]\n[rdu]\n[buf]\n")
		hosts = {"tyo": "tyo-host", "sin": "sin-host", "rdu": HOST}
		addresses = {"tyo": "100.64.0.1", "sin": "100.64.0.2", "rdu": "100.64.0.3"}
		stored = {seal.TOKEN: "write-value", seal.PEER_TOKEN: "peer-value"}
		patches = [
			mock.patch.object(seal.database, "NODES", nodes),
			mock.patch.object(seal, "cores", lambda: ["tyo", "sin", "rdu"]),
			mock.patch.object(seal, "host_token", hosts.get),
			mock.patch.object(seal.database, "tailnet_ipv4", addresses.__getitem__),
			mock.patch.object(seal.database, "stored_secret", lambda key, dry: stored[key]),
			# Nothing here may reach sops or a real secrets.json.
			mock.patch.object(seal.subprocess, "run", mock.Mock(side_effect=AssertionError("ran a program"))),
		]
		for patch in patches:
			patch.start()
			self.addCleanup(patch.stop)

	def tearDown(self):
		self.scratch.cleanup()

	def quietly(self, *arguments, dry=False):
		out = io.StringIO()
		with contextlib.redirect_stdout(out):
			seal.token(list(arguments), dry)
		return out.getvalue()

	def test_a_node_token_is_the_hmac_of_its_host_token(self):
		self.assertEqual(seal.node_token(HOST), READ)
		pairs, missing = seal.node_tokens(["rdu", "buf"], {"rdu": HOST}.get)
		self.assertEqual(pairs, f"rdu:{DIGEST}")
		self.assertEqual(missing, ["buf"])

	def test_every_core_is_given_the_tokens_and_the_cores(self):
		with mock.patch.object(seal.database, "upsert", return_value=True) as wrote:
			said = self.quietly()
		calls = [call.args for call in wrote.call_args_list]
		self.assertEqual([(node, file) for node, _, file, _ in calls], [
			("tyo", "secret.env"), ("tyo", "config.env"),
			("sin", "secret.env"), ("sin", "config.env"),
			("rdu", "secret.env"), ("rdu", "config.env"),
		])
		secret = calls[0][3]
		self.assertEqual(secret[seal.TOKEN], "write-value")
		self.assertEqual(secret[seal.PEER_TOKEN], "peer-value")
		nodes = secret["SECRETS_NODE_TOKENS"].split()
		self.assertEqual([pair.split(":")[0] for pair in nodes], ["rdu", "sin", "tyo"])
		self.assertIn(f"rdu:{DIGEST}", nodes)
		self.assertEqual(
			calls[1][3], {"CORES": "100.64.0.1 100.64.0.2 100.64.0.3", "SECRETS_NODE_SCOPES": "platform"}
		)
		self.assertIn("buf has no HOST_TOKEN_BUF", said)
		for value in ["write-value", "peer-value", HOST, READ, DIGEST]:
			self.assertNotIn(value, said)

	def test_a_dry_run_reaches_no_core_and_prints_no_secret(self):
		explode = mock.Mock(side_effect=AssertionError("reached a core"))
		with mock.patch.object(seal.database, "upsert", explode):
			said = self.quietly(dry=True)
		self.assertIn("would set", said)
		self.assertIn("CORES=100.64.0.1 100.64.0.2 100.64.0.3", said)
		self.assertIn("SECRETS_NODE_SCOPES=platform", said)
		for value in ["write-value", "peer-value", HOST, READ, DIGEST]:
			self.assertNotIn(value, said)

	def test_it_takes_nothing_and_stops_at_a_core_it_cannot_write(self):
		with mock.patch.object(seal.database, "upsert") as wrote, self.assertRaises(SystemExit):
			self.quietly("tyo")
		wrote.assert_not_called()
		with mock.patch.object(seal.database, "upsert", return_value=False), self.assertRaises(SystemExit):
			self.quietly()


class Scope(unittest.TestCase):
	"""`seal scope`, with secrets.json, the cores and the app faked, and no program run."""

	def setUp(self):
		self.made = {}

		def stored(key, dry):
			self.made[key] = dry
			return None if dry else f"made-{key}"

		manifests = {"registry": {"placements": ["tyo", "sin", "rdu"]}, "site": {"placements": ["workers"]}}
		patches = [
			mock.patch.object(seal, "cores", lambda: ["tyo", "sin", "rdu"]),
			mock.patch.object(seal.database, "stored_secret", stored),
			mock.patch.object(seal.database, "manifest_of", manifests.__getitem__),
			mock.patch.dict(
				seal.os.environ,
				{"SECRETS_SCOPE_TOKEN_CANMI__WEB": "canmi-token", "SECRETS_SCOPE_TOKEN_OLD": "ignored"},
			),
			mock.patch.object(seal.subprocess, "run", mock.Mock(side_effect=AssertionError("ran a program"))),
		]
		for patch in patches:
			patch.start()
			self.addCleanup(patch.stop)

	def quietly(self, *arguments, dry=False):
		out = io.StringIO()
		with contextlib.redirect_stdout(out):
			seal.scope(list(arguments), dry)
		return out.getvalue()

	def test_a_scope_and_app_read_back_from_their_key(self):
		for scope, app in [("accounts", "registry"), ("my-scope", "my-app"), ("a1", "b")]:
			self.assertEqual(seal.held_of_key(seal.scope_key(scope, app)), (scope, app))
		self.assertEqual(seal.scope_key("my-scope", "my-app"), "SECRETS_SCOPE_TOKEN_MY_SCOPE__MY_APP")
		for key in ["SECRETS_SCOPE_TOKEN_CANMI", "SECRETS_SCOPE_TOKEN_A____B", "SECRETS_SCOPE_TOKEN___B"]:
			self.assertIsNone(seal.held_of_key(key), key)

	def test_every_core_is_given_every_scope_token_as_a_digest(self):
		with mock.patch.object(seal.database, "upsert", return_value=True) as wrote:
			said = self.quietly("accounts", "registry")
		self.assertEqual(self.made, {"SECRETS_SCOPE_TOKEN_ACCOUNTS__REGISTRY": False})
		calls = [call.args for call in wrote.call_args_list]
		self.assertEqual([(node, app, file) for node, app, file, _ in calls], [
			("tyo", "secrets", "secret.env"), ("sin", "secrets", "secret.env"), ("rdu", "secrets", "secret.env"),
		])
		digest = lambda token: hashlib.sha256(token.encode()).hexdigest()
		made = "made-SECRETS_SCOPE_TOKEN_ACCOUNTS__REGISTRY"
		expected = f"accounts/registry:{digest(made)} canmi/web:{digest('canmi-token')}"
		self.assertEqual(calls[0][3], {"SECRETS_SCOPE_TOKENS": expected})
		for value in [made, "canmi-token", digest("canmi-token")]:
			self.assertNotIn(value, said)

	def test_the_app_named_is_given_the_token_on_each_of_its_nodes(self):
		with mock.patch.object(seal.database, "upsert", return_value=True) as wrote:
			said = self.quietly("accounts", "identities", "--app", "registry")
		given = [call.args for call in wrote.call_args_list if call.args[1] == "registry"]
		self.assertEqual([node for node, *_ in given], ["tyo", "sin", "rdu"])
		token = "made-SECRETS_SCOPE_TOKEN_ACCOUNTS__IDENTITIES"
		self.assertEqual(given[0][2:], ("secret.env", {"SECRETS_SCOPE_TOKEN": token}))
		self.assertIn("registry reads its token, writing accounts/identities, as SECRETS_SCOPE_TOKEN", said)
		self.assertNotIn("made-", said)

	def test_a_dry_run_reaches_no_machine_and_prints_no_token(self):
		explode = mock.Mock(side_effect=AssertionError("reached a core"))
		with mock.patch.object(seal.database, "upsert", explode):
			said = self.quietly("accounts", "registry", "--app", "registry", dry=True)
		self.assertEqual(self.made, {"SECRETS_SCOPE_TOKEN_ACCOUNTS__REGISTRY": True})
		self.assertIn(
			"would set in /data/apps/secrets/secret.env: SECRETS_SCOPE_TOKENS=<digests of accounts/registry,"
			" canmi/web>",
			said,
		)
		self.assertIn("would set in /data/apps/registry/secret.env: SECRETS_SCOPE_TOKEN=<redacted>", said)
		self.assertNotIn("canmi-token", said)

	def test_what_is_not_a_scope_and_app_or_an_app_on_nodes_is_refused(self):
		for arguments in [
			[],
			["accounts"],
			["Accounts", "registry"],
			["accounts", "-registry"],
			["accounts", "my--app"],
			["a--b", "registry"],
			["accounts", "registry", "web"],
			["accounts", "registry", "--app"],
			["accounts", "registry", "--app", "site"],
		]:
			with mock.patch.object(seal.database, "upsert") as wrote, self.assertRaises(SystemExit):
				self.quietly(*arguments)
			wrote.assert_not_called()
			self.assertEqual(self.made, {}, arguments)
		with mock.patch.object(seal.database, "upsert", return_value=False), self.assertRaises(SystemExit):
			self.quietly("accounts", "registry")

if __name__ == "__main__":
	unittest.main()
