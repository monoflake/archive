"""database's `commit`, which sets how the running cluster confirms a commit:
`python3 .mise/tasks/database_test.py`."""

import contextlib
import importlib.machinery
import importlib.util
import io
import json
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
loader = importlib.machinery.SourceFileLoader("task_database", str(HERE / "database"))
database = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
loader.exec_module(database)

NODES = ["tyo", "sin", "rdu", "buf", "gvx", "sha"]


def member(name, role="replica", state="streaming", nosync=None):
	tags = {"nosync": True} if nosync or (nosync is None and name not in database.ORDER) else {}
	return {"name": name, "role": role, "state": state, "timeline": 7, "tags": tags}


def view(*overrides, leader="tyo"):
	"""Every member, `leader` leading, each that never leads tagged nosync, unless overridden."""
	members = {name: member(name) for name in NODES}
	members[leader] = member(leader, "leader", "running")
	for changed in overrides:
		if changed.get("absent"):
			members.pop(changed["name"])
		else:
			members[changed["name"]] = changed
	return {"members": list(members.values())}


class Refusal(unittest.TestCase):
	def test_a_cluster_ready_for_it_is_not_refused(self):
		self.assertIsNone(database.commit_refusal(view(), NODES, "quorum"))
		self.assertIsNone(database.commit_refusal(view(), NODES, "async"))

	def test_a_member_elsewhere_still_untagged_would_confirm_commits(self):
		reason = database.commit_refusal(view(member("buf", nosync=False)), NODES, "quorum")
		self.assertIn("buf would confirm commits", reason)
		# The way back needs no tag.
		self.assertIsNone(database.commit_refusal(view(member("buf", nosync=False)), NODES, "async"))

	def test_every_member_must_answer_so_none_is_passed_over(self):
		reason = database.commit_refusal(view({"name": "sha", "absent": True}), NODES, "quorum")
		self.assertIn("sha not in Patroni's view", reason)

	def test_no_other_core_streaming_is_refused(self):
		down = view(member("sin", state="stopped"), {"name": "rdu", "absent": True})
		self.assertIn("rdu not in", database.commit_refusal(down, NODES, "quorum"))
		stopped = view(member("sin", state="stopped"), member("rdu", state="in archive recovery"))
		self.assertIn("no other core streams from tyo", database.commit_refusal(stopped, NODES, "quorum"))
		# One is enough, whichever leads.
		one = view(member("tyo", state="stopped"), member("rdu", state="streaming"), leader="sin")
		self.assertIsNone(database.commit_refusal(one, NODES, "quorum"))

	def test_a_leaderless_cluster_is_refused(self):
		leaderless = view(member("tyo", state="stopped"))
		self.assertIn("no leader", database.commit_refusal(leaderless, NODES, "quorum"))


class Modes(unittest.TestCase):
	def test_quorum_waits_for_one_other_core_and_never_goes_asynchronous(self):
		quorum = database.COMMIT_MODES["quorum"]
		self.assertEqual(quorum["synchronous_mode"], "quorum")
		self.assertIs(quorum["synchronous_mode_strict"], True)
		self.assertEqual(quorum["synchronous_node_count"], 1)
		self.assertEqual(quorum["postgresql"]["parameters"]["synchronous_commit"], "on")

	def test_async_is_the_way_back(self):
		self.assertEqual(
			database.COMMIT_MODES["async"],
			{"synchronous_mode": False, "synchronous_mode_strict": False},
		)

	def test_the_view_shows_which_is_in_force(self):
		confirmed = view(member("sin", "quorum_standby"), member("rdu", "quorum_standby"))
		self.assertTrue(database.commit_shows(confirmed, "quorum"))
		self.assertFalse(database.commit_shows(confirmed, "async"))
		self.assertFalse(database.commit_shows(view(), "quorum"))
		self.assertTrue(database.commit_shows(view(), "async"))


class Answer:
	def __init__(self, body):
		self.body = body

	def __enter__(self):
		return io.BytesIO(json.dumps(self.body).encode())

	def __exit__(self, *_):
		return False


class Commit(unittest.TestCase):
	def run_commit(self, arguments, dry, environment=None):
		out = io.StringIO()
		with (
			mock.patch.object(database, "placements", return_value=NODES),
			mock.patch.dict(database.os.environ, environment or {}, clear=False),
			contextlib.redirect_stdout(out),
		):
			database.commit(arguments, dry)
		return out.getvalue()

	def test_a_dry_run_touches_nothing_and_prints_no_secret(self):
		with (
			mock.patch.object(database.urllib.request, "urlopen") as urlopen,
			mock.patch.object(database, "tailnet_ipv4") as tailnet,
		):
			printed = self.run_commit(["quorum"], True, {"DATABASE_PATRONI_PASSWORD": "hunter2"})
		urlopen.assert_not_called()
		tailnet.assert_not_called()
		self.assertNotIn("hunter2", printed)
		self.assertIn('"synchronous_mode": "quorum"', printed)
		self.assertIn("buf, gvx, sha is tagged nosync", printed)
		self.assertNotIn("nosync", self.run_commit(["async"], True))

	def test_only_a_mode_it_knows(self):
		for arguments in ([], ["sync"], ["quorum", "async"]):
			with self.assertRaises(SystemExit):
				self.run_commit(arguments, True)

	def test_patches_the_leaders_config_and_waits_until_the_view_shows_it(self):
		sent = []

		def urlopen(request, timeout):
			sent.append(request)
			return Answer({"synchronous_mode": "quorum", "synchronous_mode_strict": True, "ttl": 30})

		before = view(leader="sin")
		after = view(member("tyo", "quorum_standby"), member("rdu", "quorum_standby"), leader="sin")
		with (
			mock.patch.object(database, "tailnet_ipv4", side_effect=lambda n: f"100.64.0.{NODES.index(n)}"),
			mock.patch.object(database, "cluster", side_effect=[before, before, after]),
			mock.patch.object(database.urllib.request, "urlopen", side_effect=urlopen),
			mock.patch.object(database.time, "sleep"),
		):
			printed = self.run_commit(["quorum"], False, {"DATABASE_PATRONI_PASSWORD": "hunter2"})
		(request,) = sent
		self.assertEqual(request.get_method(), "PATCH")
		# sin leads, and the mock gives each node the address of its place in NODES.
		sin = f"100.64.0.{NODES.index('sin')}"
		self.assertEqual(request.full_url, f"http://{sin}:{database.REST}/config")
		self.assertEqual(json.loads(request.data), database.COMMIT_MODES["quorum"])
		self.assertTrue(request.get_header("Authorization").startswith("Basic "))
		self.assertIn("quorum in force", printed)
		self.assertNotIn("hunter2", printed)

	def test_a_refused_cluster_is_never_patched(self):
		untagged = view(member("gvx", nosync=False))
		with (
			mock.patch.object(database, "tailnet_ipv4", return_value="100.64.0.9"),
			mock.patch.object(database, "cluster", return_value=untagged),
			mock.patch.object(database.urllib.request, "urlopen") as urlopen,
			self.assertRaises(SystemExit) as stopped,
		):
			self.run_commit(["quorum"], False, {"DATABASE_PATRONI_PASSWORD": "rest"})
		urlopen.assert_not_called()
		self.assertIn("gvx would confirm commits", str(stopped.exception))


class Grant(unittest.TestCase):
	def run_grant(self, app, dry, placements):
		out = io.StringIO()
		with (
			mock.patch.object(database, "placed_on", return_value=placements),
			mock.patch.object(database, "placements", return_value=NODES),
			mock.patch.object(database, "stored_secret", return_value="hunter2"),
			mock.patch.object(
				database, "tailnet_ipv4", side_effect=lambda node: f"100.64.0.{NODES.index(node) + 1}"
			),
			mock.patch.object(database, "cluster", return_value=view()),
			mock.patch.object(database, "ssh", return_value="") as ssh,
			mock.patch.object(database, "upsert", return_value=True) as upsert,
			contextlib.redirect_stdout(out),
		):
			database.grant(["platform", app], dry)
		return out.getvalue(), ssh, upsert

	def test_the_ledger_is_told_its_own_nodes_patroni_beside_its_url(self):
		_, _, upsert = self.run_grant("ledger", False, ["tyo", "sin", "rdu"])
		written = {(c.args[0], c.args[2]): c.args[3] for c in upsert.call_args_list}
		for node in ["tyo", "sin", "rdu"]:
			self.assertIn("DATABASE_URL", written[(node, "secret.env")])
			address = f"100.64.0.{NODES.index(node) + 1}"
			self.assertEqual(
				written[(node, "config.env")], {"PATRONI_PRIMARY_URL": f"http://{address}:8008/primary"}
			)

	def test_another_app_is_given_no_gate(self):
		_, _, upsert = self.run_grant("registry", False, ["tyo", "sin", "rdu"])
		self.assertEqual({c.args[2] for c in upsert.call_args_list}, {"secret.env"})

	def test_a_dry_run_says_both_and_touches_and_prints_nothing_secret(self):
		printed, ssh, upsert = self.run_grant("ledger", True, ["tyo", "sin", "rdu"])
		ssh.assert_not_called()
		upsert.assert_not_called()
		self.assertNotIn("hunter2", printed)
		for node, address in [("tyo", "100.64.0.1"), ("sin", "100.64.0.2"), ("rdu", "100.64.0.3")]:
			self.assertIn(
				f"config.env on {node}: PATRONI_PRIMARY_URL=http://{address}:8008/primary", printed
			)



class Placed(unittest.TestCase):
	"""Which nodes an app is placed on, asked of control by id and answered by name."""

	IDS = {"tyo": "0000000000000001", "sin": "0000000000000009", "rdu": "0000000000000007"}

	def setUp(self):
		self.scratch = tempfile.TemporaryDirectory()
		self.addCleanup(self.scratch.cleanup)
		nodes = Path(self.scratch.name) / "nodes.toml"
		nodes.write_text("".join(f'[{name}]\nid = "{node}"\n' for name, node in self.IDS.items()))
		patches = [
			mock.patch.object(database, "NODES", nodes),
			mock.patch.dict(database.os.environ, {database.READ_TOKEN: "read-s3cret"}),
		]
		for patch in patches:
			patch.start()
			self.addCleanup(patch.stop)

	def answering(self, placements):
		sent = []

		def urlopen(request, timeout):
			sent.append(request)
			spec = {"placements": placements}
			return Answer({"status": "success", "data": {"name": "database", "revision": 3, "spec": spec}})

		return sent, mock.patch.object(database.urllib.request, "urlopen", side_effect=urlopen)

	def test_names_each_node_control_places_it_on_in_its_order(self):
		sent, urlopen = self.answering([self.IDS["sin"], self.IDS["tyo"]])
		with urlopen:
			self.assertEqual(database.placed_on("database"), ["sin", "tyo"])
		(request,) = sent
		self.assertEqual(request.full_url, f"{database.CONTROL}/v1/registry/apps/database")
		self.assertEqual(request.get_header("Authorization"), "Bearer read-s3cret")

	def test_a_worker_is_placed_on_none(self):
		_, urlopen = self.answering([])
		with urlopen:
			self.assertEqual(database.placed_on("site"), [])

	def test_an_id_nodes_toml_does_not_name_or_no_token_fails(self):
		_, urlopen = self.answering(["0000000000000000"])
		with urlopen, self.assertRaises(SystemExit) as stopped:
			database.placed_on("database")
		self.assertIn("0000000000000000", str(stopped.exception))
		with (
			mock.patch.dict(database.os.environ, {database.READ_TOKEN: ""}),
			mock.patch.object(database.urllib.request, "urlopen") as urlopen,
			self.assertRaises(SystemExit),
		):
			database.placed_on("database")
		urlopen.assert_not_called()


if __name__ == "__main__":
	unittest.main()
