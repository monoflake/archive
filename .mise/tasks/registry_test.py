"""registry's request body from one nodes.toml table: `python3 .mise/tasks/registry_test.py`."""

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


if __name__ == "__main__":
	unittest.main()
