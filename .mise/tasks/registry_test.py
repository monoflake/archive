"""registry's request bodies, from a nodes.toml table and from each app's placements:
`python3 .mise/tasks/registry_test.py`."""

import contextlib
import importlib.machinery
import importlib.util
import io
import os
import tempfile
import tomllib
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
loader = importlib.machinery.SourceFileLoader("task_registry", str(HERE / "registry"))
registry = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
loader.exec_module(registry)

NODE = tomllib.loads(
	'[tyo]\ncore = true\ntier = "datacenter"\ndomain = "oci"\nexpiry = 2036\nsystem = "debian"\n'
	'grants = ["hub:peer"]\n'
)["tyo"]


class Body(unittest.TestCase):
	def test_the_table_is_kept_whole_and_the_address_added(self):
		body = registry.request_body(NODE, "100.64.0.1")
		self.assertEqual(body["by"], "import")
		self.assertEqual(body["node"], {**NODE, "address": "100.64.0.1"})
		self.assertEqual(body["node"]["grants"], ["hub:peer"])

	def test_the_table_given_is_not_changed(self):
		before = dict(NODE)
		registry.request_body(NODE, "100.64.0.1")
		self.assertEqual(NODE, before)

	def test_optional_keys_stay_absent_when_the_table_lacks_them(self):
		table = {"tier": "datacenter", "domain": "oci", "system": "alpine"}
		self.assertEqual(
			registry.request_body(table, "100.64.0.2")["node"],
			{"tier": "datacenter", "domain": "oci", "system": "alpine", "address": "100.64.0.2"},
		)


class Placements(unittest.TestCase):
	def test_a_list_of_node_names_is_kept_in_order(self):
		self.assertEqual(registry.placed({"placements": ["tyo", "sin", "rdu"]}), ["tyo", "sin", "rdu"])

	def test_what_the_registry_keeps_nothing_of_is_skipped(self):
		for declaration in [
			{},
			{"placements": []},
			{"placements": ["workers"]},
			{"placements": ["tyo", "workers"]},
			{"placements": ["TYO"]},
			{"placements": ["tyo1"]},
			{"placements": ["sín"]},
			{"placements": [3]},
			{"placements": "tyo"},
		]:
			self.assertIsNone(registry.placed(declaration), declaration)

	def test_every_app_is_named_once_in_order(self):
		declarations = [
			("a/service.toml", {"name": "web", "placements": ["tyo"]}),
			("b/service.toml", {"name": "aka", "placements": ["workers"]}),
		]
		self.assertEqual(registry.apps_placed(declarations), [("web", ["tyo"]), ("aka", None)])

	def test_a_name_declared_twice_or_not_at_all_fails(self):
		twice = [("a", {"name": "web", "placements": ["tyo"]}), ("b", {"name": "web"})]
		for declarations in [twice, [("a", {"placements": ["tyo"]})]]:
			with self.assertRaises(SystemExit):
				registry.apps_placed(declarations)

	def test_the_body_carries_the_revision_read(self):
		self.assertEqual(
			registry.app_body(7, ["tyo", "sin"]),
			{"by": "import", "revision": 7, "spec": {"placements": ["tyo", "sin"]}},
		)

	def test_an_app_the_registry_does_not_hold_is_at_revision_zero(self):
		held = {"status": "success", "data": {"kind": "app", "name": "web", "revision": 12}}
		absent = {"status": "error", "code": "no_such_resource", "message": "No resource has this id"}
		self.assertEqual(registry.revision_of(held, "web"), 12)
		self.assertEqual(registry.revision_of(absent, "web"), 0)
		with self.assertRaises(SystemExit):
			registry.revision_of({"status": "error", "code": "store_unavailable"}, "web")


class Sources(unittest.TestCase):
	def test_deploy_sources_are_read_without_running_the_task(self):
		text = (
			"import host_api\n"
			"HOME_OWNER = 'monoflake'\n"
			'DEPLOY_SOURCES = "monoflake/infra=infra monoflake/platform=platform canmi21/cue=canmi"\n'
		)
		self.assertEqual(
			registry.deploy_sources(text), ["monoflake/infra", "monoflake/platform", "canmi21/cue"]
		)

	def test_a_task_without_them_fails(self):
		for text in ["HOME_OWNER = 'monoflake'\n", "DEPLOY_SOURCES = ' '.join(['a/b=c'])\n"]:
			with self.assertRaises(SystemExit):
				registry.deploy_sources(text)


def write(path, text):
	path.parent.mkdir(parents=True, exist_ok=True)
	path.write_text(text)


class Jj(unittest.TestCase):
	def run_jj(self, returncode, stdout="", stderr=""):
		ran = mock.Mock(returncode=returncode, stdout=stdout, stderr=stderr)
		with mock.patch.object(registry.subprocess, "run", return_value=ran) as run:
			answer = registry.jj(Path("/w/repos/cue"), "file", "show", "-r", "main", "a")
		return answer, run.call_args

	def test_it_reads_main_without_snapshotting_the_working_copy(self):
		answer, call = self.run_jj(0, stdout="name = 'hub'\n")
		self.assertEqual(answer, "name = 'hub'\n")
		jj = ["jj", "--ignore-working-copy", "-R", "/w/repos/cue"]
		self.assertEqual(call.args[0], [*jj, "file", "show", "-r", "main", "a"])
		self.assertEqual(call.kwargs["cwd"], Path("/w/repos/cue"))

	def test_what_jj_refuses_fails_with_what_it_said(self):
		with self.assertRaises(SystemExit) as failed:
			self.run_jj(1, stderr="Error: Revision `main` doesn't exist\nHint: ...\n")
		self.assertIn("/w/repos/cue: Error: Revision `main` doesn't exist", str(failed.exception.code))


class Importing(unittest.TestCase):
	"""`import` run whole over a workspace made for it: repos.toml, three checkouts on disk, and what
	each one's `main` holds -- infra's node task, and cue's declarations one level shallower than the
	others', as they are -- served by a jj stood in for. Every machine it would reach is stood in for
	too, so a name it shadows, a source it misses or a branch it breaks fails here rather than on the
	operator's terminal."""

	NODES = {"tyo": {**NODE}, "sin": {"tier": "datacenter", "domain": "oci", "system": "debian"}}

	def setUp(self):
		made = tempfile.TemporaryDirectory()
		self.addCleanup(made.cleanup)
		self.workspace = Path(made.name)
		write(
			self.workspace / "repos.toml",
			'[repos]\ninfra = "monoflake:infra"\nplatform = "monoflake:platform"\ncue = "canmi21:cue"\n'
			'web = "canmi21:web"\n',
		)
		# Each checkout's `main`, by its directory: path to what the bookmark holds there.
		self.main = {"infra": {}, "platform": {}, "cue": {}}
		for checkout in self.main:
			(self.workspace / "repos" / checkout).mkdir(parents=True)
		self.main["infra"][".mise/tasks/node"] = (
			'DEPLOY_SOURCES = "monoflake/infra=infra monoflake/platform=platform canmi21/cue=canmi"\n'
		)
		self.declare("infra", "apps/network/caddy", "caddy", ["tyo", "sin"])
		self.declare("platform", "apps/site/web", "web", ["tyo", "sin"])
		self.declare("platform", "apps/edge/aka", "aka", ["workers"])
		self.declare("platform", "apps/site/web/node_modules/x", "x", ["tyo"])
		self.declare("cue", "apps/hub", "hub", ["tyo"])

	def declare(self, checkout, directory, name, placements, committed=True):
		"""A service.toml on `checkout`'s main, or in its working tree alone where not committed."""
		listed = ", ".join(f'"{node}"' for node in placements)
		text = f'name = "{name}"\nplacements = [{listed}]\n'
		path = f"{directory}/service.toml"
		if committed:
			self.main[checkout][path] = text
		else:
			write(self.workspace / "repos" / checkout / path, text)

	def jj(self, root, *arguments):
		"""jj as the import runs it, over `self.main`; a checkout missing there has no `main`."""
		tree = self.main.get(Path(root).name)
		if tree is None or arguments[2:4] != ("-r", "main"):
			registry.fail(f"jj {' '.join(arguments)} in {root}: Error: Revision `main` doesn't exist")
		if arguments[:2] == ("file", "list"):
			self.assertEqual(arguments[4], 'root-glob:"apps/**/service.toml"')
			declared = [path for path in tree if path.startswith("apps/")]
			return "".join(f"{path}\n" for path in declared if path.endswith("/service.toml"))
		self.assertEqual(arguments[:2], ("file", "show"))
		return tree[arguments[4]]

	def run_import(self, dry, answered=None):
		addresses = {"tyo": "100.64.0.1", "sin": "100.64.0.2"}
		printed = io.StringIO()
		with contextlib.ExitStack() as stack:
			stack.enter_context(mock.patch.object(registry, "WORKSPACE", self.workspace))
			stack.enter_context(mock.patch.object(registry, "jj", self.jj))
			stack.enter_context(mock.patch.object(registry, "load_nodes", lambda: self.NODES))
			stack.enter_context(mock.patch.object(registry, "placements", lambda: ["tyo", "sin"]))
			stack.enter_context(
				mock.patch.object(registry.database, "tailnet_ipv4", addresses.__getitem__)
			)
			stack.enter_context(mock.patch.dict(os.environ, {registry.TOKEN: "s3cret"}))
			if answered is not None:
				stack.enter_context(mock.patch.object(registry, "answered", answered))
			stack.enter_context(contextlib.redirect_stdout(printed))
			registry.importing(dry)
		return printed.getvalue().splitlines()

	def failure(self):
		"""What the import fails with, before it would write anything."""
		with self.assertRaises(SystemExit) as failed:
			self.run_import(True)
		return str(failed.exception.code)

	def test_a_dry_run_says_every_write_and_makes_none(self):
		def unreachable(*_):
			raise AssertionError("a dry run asked a registry")

		lines = self.run_import(True, unreachable)
		puts = [line.split()[1] for line in lines if line.startswith("  PATCH /v1/apps/")]
		self.assertEqual(puts, ["/v1/apps/caddy", "/v1/apps/web", "/v1/apps/hub"])
		web = registry.app_body("<read>", ["tyo", "sin"])
		self.assertIn(f"  PATCH /v1/apps/web {registry.json.dumps(web)}", lines)
		self.assertIn("  skip aka", lines)
		self.assertEqual(sum(line.startswith("  PUT /nodes/") for line in lines), 2)
		self.assertNotIn("s3cret", "\n".join(lines))

	def test_a_run_puts_each_app_at_the_revision_it_read(self):
		asked = []

		def answered(addresses, method, path, body=None, secret=None):
			asked.append((addresses[0], method, path, body, secret))
			if method == "GET":
				return {"status": "error", "code": "no_such_resource"}
			if path.startswith("/v1/apps/"):
				return {"status": "success", "data": {"revision": 9}}
			return {"status": "success", "data": {}}

		lines = self.run_import(False, answered)
		self.assertIn("registry: app hub: changed, revision 9", lines)
		self.assertIn("registry: app aka: skipped, not placed on nodes", lines)
		self.assertIn("registry: tyo: unchanged", lines)
		put = [entry for entry in asked if entry[1:3] == ("PUT", "/v1/apps/web")]
		web = registry.app_body(0, ["tyo", "sin"])
		self.assertEqual(put, [("100.64.0.1", "PUT", "/v1/apps/web", web, "s3cret")])

	def test_a_rerun_keeps_the_release_an_app_already_has(self):
		"""Against a registry answering as the registry does: `PUT` sets a spec whole, `PATCH` sets
		the keys given and keeps the rest, each held to the revision read."""
		release = {"repository": "monoflake/platform", "run": 7, "commit": "0" * 40}
		held = {"web": {"revision": 4, "spec": {"placements": ["tyo"], "release": release}}}
		written = []

		def answered(addresses, method, path, body=None, secret=None):
			if not path.startswith("/v1/apps/"):
				return {"status": "success", "data": {}}
			name = path.removeprefix("/v1/apps/")
			app = held.get(name)
			if method == "GET":
				if app is None:
					return {"status": "error", "code": "no_such_resource"}
				return {"status": "success", "data": {"name": name, **app}}
			written.append((method, name))
			if body["revision"] != (app["revision"] if app else 0):
				return {"status": "error", "code": "invalid_revision"}
			spec = {**app["spec"], **body["spec"]} if method == "PATCH" else body["spec"]
			held[name] = {"revision": 10 + len(written), "spec": spec}
			return {"status": "success", "data": {"revision": held[name]["revision"]}}

		self.run_import(False, answered)
		self.assertEqual(held["web"]["spec"], {"placements": ["tyo", "sin"], "release": release})
		self.assertEqual(written, [("PUT", "caddy"), ("PATCH", "web"), ("PUT", "hub")])

	def test_an_app_held_is_patched_and_one_not_held_is_put(self):
		self.assertEqual(registry.method_at(0), "PUT")
		self.assertEqual(registry.method_at(12), "PATCH")

	def test_a_source_not_cloned_fails_rather_than_leaving_its_apps_out(self):
		(self.workspace / "repos/cue").rename(self.workspace / "elsewhere")
		self.assertIn("canmi21/cue", self.failure())

	def test_a_source_repos_toml_does_not_name_fails(self):
		write(self.workspace / "repos.toml", '[repos]\ninfra = "monoflake:infra"\n')
		self.assertIn("monoflake/platform", self.failure())

	def test_one_name_in_two_repositories_fails(self):
		self.declare("cue", "apps/web", "web", ["sin"])
		self.assertIn("web is declared twice", self.failure())

	def test_what_is_not_committed_is_not_imported(self):
		self.declare("platform", "apps/data/secrets", "secrets", ["tyo"], committed=False)
		self.declare("cue", "apps/hub", "hub", ["sin"], committed=False)
		lines = self.run_import(True)
		self.assertNotIn("secrets", "\n".join(lines))
		hub = registry.app_body("<read>", ["tyo"])
		self.assertIn(f"  PATCH /v1/apps/hub {registry.json.dumps(hub)}", lines)

	def test_a_checkout_without_main_fails_naming_it(self):
		del self.main["cue"]
		failed = self.failure()
		self.assertIn(str(self.workspace / "repos/cue"), failed)
		self.assertIn("`main`", failed)



class Widened(unittest.TestCase):
	def test_what_it_names_is_kept_in_order_and_the_rest_added_once(self):
		self.assertEqual(
			registry.widened(
				"monoflake/platform canmi21/web",
				["monoflake/infra", "monoflake/platform", "canmi21/cue"],
			),
			"monoflake/platform canmi21/web monoflake/infra canmi21/cue",
		)

	def test_nothing_named_yet_is_the_sources_alone(self):
		self.assertEqual(registry.widened("", ["monoflake/infra"]), "monoflake/infra")

	def test_a_source_named_twice_is_named_once(self):
		self.assertEqual(registry.widened("a/b a/b", ["a/b"]), "a/b")


class Token(unittest.TestCase):
	"""`token` run whole, every machine and every secret stood in for: the registry's two nodes,
	and the deployer on tyo with what its config.env names."""

	SECRET = "s3cret-never-printed"
	SOURCES = ["monoflake/infra", "monoflake/platform", "canmi21/cue"]

	def run_token(self, dry, current="monoflake/platform canmi21/web"):
		self.written = []
		self.asked = []

		def upsert(node, app, file, pairs):
			self.written.append((node, app, file, pairs))
			return True

		def ssh(node, command):
			self.asked.append((node, command))
			return current

		manifests = {"registry": {"placements": ["tyo", "sin"]}, "deployer": {"placements": ["tyo"]}}
		addresses = {"tyo": "100.64.0.1", "sin": "100.64.0.2"}
		printed = io.StringIO()
		database = registry.database
		with contextlib.ExitStack() as stack:
			stack.enter_context(mock.patch.object(database, "stored_secret", lambda *_: self.SECRET))
			stack.enter_context(mock.patch.object(database, "manifest_of", manifests.__getitem__))
			stack.enter_context(mock.patch.object(database, "tailnet_ipv4", addresses.__getitem__))
			stack.enter_context(mock.patch.object(database, "upsert", upsert))
			stack.enter_context(mock.patch.object(database, "ssh", ssh))
			stack.enter_context(mock.patch.object(registry, "host_sources", lambda: self.SOURCES))
			stack.enter_context(contextlib.redirect_stdout(printed))
			registry.token(dry)
		return printed.getvalue()

	def test_a_dry_run_says_every_write_asks_nothing_and_prints_no_secret(self):
		printed = self.run_token(True)
		self.assertEqual(self.written, [])
		self.assertEqual(self.asked, [])
		self.assertNotIn(self.SECRET, printed)
		for line in [
			"registry: tyo: would set, keeping every other line, mode 600, in"
			" /data/apps/registry/secret.env: REGISTRY_TOKEN=<redacted>",
			"registry: sin: would set, keeping every other line, mode 600, in"
			" /data/apps/registry/secret.env: REGISTRY_TOKEN=<redacted>",
			"registry: tyo: would set in /data/apps/deployer/secret.env: REGISTRY_TOKEN=<redacted>",
			"registry: tyo: would set in /data/apps/deployer/config.env:"
			" REGISTRY=100.64.0.1 100.64.0.2",
			"registry: tyo: would set in /data/apps/deployer/config.env: DEPLOY_SOURCES=<what it"
			" names>, then each of monoflake/infra monoflake/platform canmi21/cue it does not",
		]:
			self.assertIn(line, printed.splitlines())

	def test_a_run_gives_the_registry_and_the_deployer_the_token_and_widens_the_sources(self):
		printed = self.run_token(False)
		self.assertNotIn(self.SECRET, printed)
		token = {"REGISTRY_TOKEN": self.SECRET}
		self.assertEqual(
			self.written,
			[
				("tyo", "registry", "secret.env", token),
				("sin", "registry", "secret.env", token),
				("tyo", "deployer", "secret.env", token),
				(
					"tyo",
					"deployer",
					"config.env",
					{
						"REGISTRY": "100.64.0.1 100.64.0.2",
						"DEPLOY_SOURCES": "monoflake/platform canmi21/web monoflake/infra canmi21/cue",
					},
				),
			],
		)
		[(node, command)] = self.asked
		self.assertEqual(node, "tyo")
		self.assertIn("/data/apps/deployer/config.env", command)
		self.assertIn("s/^DEPLOY_SOURCES=//p", command)

	def test_a_deployer_that_names_no_sources_is_given_hosts(self):
		self.run_token(False, current="")
		self.assertEqual(self.written[-1][3]["DEPLOY_SOURCES"], " ".join(self.SOURCES))

	def test_its_sources_unread_writes_nothing_to_it(self):
		with self.assertRaises(SystemExit) as failed:
			self.run_token(False, current=None)
		self.assertIn("reading DEPLOY_SOURCES of deployer failed on tyo", str(failed.exception.code))
		self.assertEqual([app for _, app, _, _ in self.written], ["registry", "registry"])

	def test_host_sources_are_read_from_infra_main(self):
		made = tempfile.TemporaryDirectory()
		self.addCleanup(made.cleanup)
		workspace = Path(made.name)
		(workspace / "repos/infra").mkdir(parents=True)
		node = 'DEPLOY_SOURCES = "monoflake/infra=infra canmi21/cue=canmi"\n'
		with mock.patch.object(registry, "committed", lambda root, path: node):
			self.assertEqual(registry.host_sources(workspace), ["monoflake/infra", "canmi21/cue"])


if __name__ == "__main__":
	unittest.main()
