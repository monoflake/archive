"""registry's request bodies, from a nodes.toml table and from each app's placements:
`python3 .mise/tasks/registry_test.py`."""

import importlib.machinery
import importlib.util
import tomllib
import unittest
from pathlib import Path

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


if __name__ == "__main__":
	unittest.main()
