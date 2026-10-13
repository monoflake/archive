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


class Importing(unittest.TestCase):
	"""`import` run whole over a workspace made for it: infra's node task, repos.toml and three
	checkouts, cue's declarations one level shallower than the others', as they are. Every machine it
	would reach is stood in for, so a name it shadows, a source it misses or a branch it breaks fails
	here rather than on the operator's terminal."""

	NODES = {"tyo": {**NODE}, "sin": {"tier": "datacenter", "domain": "oci", "system": "debian"}}

	def setUp(self):
		made = tempfile.TemporaryDirectory()
		self.addCleanup(made.cleanup)
		self.workspace = Path(made.name)
		repos = self.workspace / "repos"
		write(
			self.workspace / "repos.toml",
			'[repos]\ninfra = "monoflake:infra"\nplatform = "monoflake:platform"\ncue = "canmi21:cue"\n'
			'web = "canmi21:web"\n',
		)
		write(
			repos / "infra/.mise/tasks/node",
			'DEPLOY_SOURCES = "monoflake/infra=infra monoflake/platform=platform canmi21/cue=canmi"\n',
		)
		self.declare("infra/apps/network/caddy", "caddy", ["tyo", "sin"])
		self.declare("platform/apps/site/web", "web", ["tyo", "sin"])
		self.declare("platform/apps/edge/aka", "aka", ["workers"])
		self.declare("platform/apps/site/web/node_modules/x", "x", ["tyo"])
		self.declare("cue/apps/hub", "hub", ["tyo"])

	def declare(self, directory, name, placements):
		listed = ", ".join(f'"{node}"' for node in placements)
		path = self.workspace / "repos" / directory / "service.toml"
		write(path, f'name = "{name}"\nplacements = [{listed}]\n')

	def run_import(self, dry, answered=None):
		addresses = {"tyo": "100.64.0.1", "sin": "100.64.0.2"}
		printed = io.StringIO()
		with contextlib.ExitStack() as stack:
			stack.enter_context(mock.patch.object(registry, "WORKSPACE", self.workspace))
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
		puts = [line.split()[1] for line in lines if line.startswith("  PUT /v1/apps/")]
		self.assertEqual(puts, ["/v1/apps/caddy", "/v1/apps/web", "/v1/apps/hub"])
		web = registry.app_body("<read>", ["tyo", "sin"])
		self.assertIn(f"  PUT /v1/apps/web {registry.json.dumps(web)}", lines)
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

	def test_a_source_not_cloned_fails_rather_than_leaving_its_apps_out(self):
		(self.workspace / "repos/cue").rename(self.workspace / "elsewhere")
		self.assertIn("canmi21/cue", self.failure())

	def test_a_source_repos_toml_does_not_name_fails(self):
		write(self.workspace / "repos.toml", '[repos]\ninfra = "monoflake:infra"\n')
		self.assertIn("monoflake/platform", self.failure())

	def test_one_name_in_two_repositories_fails(self):
		self.declare("cue/apps/web", "web", ["sin"])
		self.assertIn("web is declared twice", self.failure())


if __name__ == "__main__":
	unittest.main()
