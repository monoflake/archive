"""platform's command line against a fake registry on 127.0.0.1, its catalog that of
apps/data/registry/src/kinds.rs: `python3 .mise/tasks/platform_test.py`."""

import contextlib
import copy
import importlib.machinery
import importlib.util
import io
import json
import os
import threading
import unittest
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
loader = importlib.machinery.SourceFileLoader("task_platform", str(HERE / "platform"))
platform = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
loader.exec_module(platform)

TOKEN = "token-for-the-fake"
SECRET_VALUE = "me-at-example-dot-com"


def field(name, type_, required=False, **more):
	return {"name": name, "type": type_, "editable": True, "required": required, **more}


CATALOG = [
	{
		"name": "node",
		"plural": "nodes",
		"fields": [
			field("address", "string", True),
			field("tier", "choice", True, choices=["datacenter", "home", "transient"]),
			field("domain", "reference", True, kind="domain"),
			field("system", "choice", True, choices=["debian", "alpine"]),
			field("core", "boolean"),
			field("expiry", "integer"),
			field("retired", "boolean"),
			field("account", "reference", kind="account"),
			field("roles", "list", choices=["compute", "storage", "core"]),
		],
		"actions": [],
		"columns": ["address", "tier", "domain", "system"],
	},
	{
		"name": "app",
		"plural": "apps",
		"fields": [field("placements", "list", True), field("release", "object")],
		"actions": [],
		"columns": ["placements"],
	},
	{
		"name": "account",
		"plural": "accounts",
		"fields": [
			field("provider", "string", True),
			field(
				"identities",
				"list",
				True,
				fields=[
					field("kind", "choice", True, choices=["user_id", "tenancy", "email"]),
					field("value", "string", secret=True),
					{**field("secret", "string"), "editable": False},
				],
			),
		],
		"actions": [],
		"columns": ["provider"],
	},
	{
		"name": "domain",
		"plural": "domains",
		"fields": [field("note", "string")],
		"actions": [],
		"columns": ["note"],
	},
]

DOMAIN_ID = "00000000000000d1"
NODE_ID = "abd3922110a78032"


class Registry:
	"""What the fake answers from, and every request it was sent."""

	def __init__(self):
		self.revision = 10
		self.held = {
			"domains": {"oci": self.resource("domain", DOMAIN_ID, "oci", {})},
			"nodes": {
				"tyo": self.resource(
					"node",
					NODE_ID,
					"tyo",
					{
						"address": "100.64.0.1",
						"tier": "datacenter",
						"domain": DOMAIN_ID,
						"system": "debian",
						"retired": False,
						"roles": ["compute", "core"],
						"grants": ["hub:peer"],
					},
				)
			},
			"apps": {},
			"accounts": {
				"oci-2": self.resource(
					"account",
					"00000000000000a1",
					"oci-2",
					{
						"provider": "oracle",
						"identities": [
							{"kind": "user_id", "secret": "accounts/registry/ACCOUNT_00000000000000A1_USER_ID"},
							{"kind": "email", "secret": "accounts/registry/ACCOUNT_00000000000000A1_EMAIL"},
						],
					},
				)
			},
		}
		self.requests = []
		self.refusal = None

	def resource(self, kind, id_, name, spec):
		return {"kind": kind, "id": id_, "name": name, "revision": self.revision, "spec": spec}

	def find(self, plural, key):
		for resource in self.held.get(plural, {}).values():
			if key in (resource["id"], resource["name"]):
				return resource
		return None

	def answer(self, method, path, authorization, body):
		self.requests.append({"method": method, "path": path, "auth": authorization, "body": body})
		parts = path.strip("/").split("/")
		if parts == ["v1", "kinds"]:
			return 200, {"status": "success", "data": {"items": CATALOG}}
		plural = parts[1]
		if plural not in self.held:
			return 404, {"status": "error", "code": "no_such_kind", "message": "No such kind"}
		if len(parts) == 2:
			items = list(self.held[plural].values())
			return 200, {"status": "success", "data": {"items": items, "revision": self.revision}}
		key = parts[2]
		found = self.find(plural, key)
		if method == "GET":
			if found is None:
				return 404, {"status": "error", "code": "no_such_resource", "message": "None held"}
			return 200, {"status": "success", "data": found}
		if authorization != f"Bearer {TOKEN}":
			return 401, {"status": "error", "code": "unauthorized", "message": "No token"}
		if self.refusal:
			return 400, {"status": "error", "code": self.refusal[0], "message": self.refusal[1]}
		at = found["revision"] if found else 0
		if body["revision"] != at:
			return 409, {"status": "error", "code": "invalid_revision", "message": "Changed since"}
		if method == "PUT":
			spec = body["spec"]
		else:
			spec = copy.deepcopy(found["spec"])
			for name, value in body["spec"].items():
				if value is None:
					spec.pop(name, None)
				else:
					spec[name] = value
		if found is not None and spec == found["spec"]:
			return 200, {"status": "success", "data": {}}
		self.revision += 1
		kind = next(kind["name"] for kind in CATALOG if kind["plural"] == plural)
		made = found or self.resource(kind, f"{self.revision:016x}", key, {})
		made.update(spec=spec, revision=self.revision)
		self.held[plural][made["name"]] = made
		data = {"revision": self.revision, "task": "00000000000000f1"}
		return 202, {"status": "success", "data": data}


def handler(state):
	class Handler(BaseHTTPRequestHandler):
		def log_message(self, *arguments):
			pass

		def respond(self):
			length = int(self.headers.get("Content-Length") or 0)
			body = json.loads(self.rfile.read(length)) if length else None
			auth = self.headers.get("Authorization")
			code, answer = state.answer(self.command, self.path, auth, body)
			data = json.dumps(answer).encode()
			self.send_response(code)
			self.send_header("Content-Type", "application/json")
			self.send_header("Content-Length", str(len(data)))
			self.end_headers()
			self.wfile.write(data)

		do_GET = do_PUT = do_PATCH = respond

	return Handler


class Command(unittest.TestCase):
	def setUp(self):
		self.state = Registry()
		self.server = ThreadingHTTPServer(("127.0.0.1", 0), handler(self.state))
		serving = threading.Thread(target=self.server.serve_forever, args=(0.01,), daemon=True)
		serving.start()
		self.addCleanup(self.server.server_close)
		self.addCleanup(self.server.shutdown)
		for patch in (
			mock.patch.object(platform.registry, "PORT", self.server.server_address[1]),
			mock.patch.object(platform, "cores", lambda: iter(["127.0.0.1"])),
			mock.patch.object(platform.registry, "placements", lambda: ["tyo", "sin", "rdu"]),
			mock.patch.dict(os.environ, {"REGISTRY_TOKEN": TOKEN}),
		):
			patch.start()
			self.addCleanup(patch.stop)
		self.asked = []

	def getpass(self, prompt):
		self.asked.append(prompt)
		return SECRET_VALUE

	def run_task(self, *arguments):
		"""What the task printed, and its exit: None, or the message it failed with."""
		out, err = io.StringIO(), io.StringIO()
		code = None
		with (
			mock.patch.object(platform.getpass, "getpass", self.getpass),
			contextlib.redirect_stdout(out),
			contextlib.redirect_stderr(err),
		):
			try:
				platform.main(list(arguments))
			except SystemExit as exited:
				code = exited.code
		return out.getvalue(), err.getvalue(), code

	def writes(self):
		return [r for r in self.state.requests if r["method"] in ("PUT", "PATCH")]

	def failed(self, *arguments):
		out, err, code = self.run_task(*arguments)
		self.assertIsInstance(code, str, "a failure exits 1 with a message")
		self.assertEqual(self.writes(), [], "a refusal before sending sends nothing")
		return code


class Reading(Command):
	def test_list_shows_the_catalog_s_columns_and_ids_by_eight_digits(self):
		out, _, code = self.run_task("nodes", "list")
		self.assertIsNone(code)
		head, row = out.splitlines()
		self.assertEqual(head.split(), ["id", "name", "address", "tier", "domain", "system"])
		shown = [NODE_ID[:8], "tyo", "100.64.0.1", "datacenter", DOMAIN_ID[:8], "debian"]
		self.assertEqual(row.split(), shown)
		self.assertTrue(all(r["auth"] is None for r in self.state.requests), "reads carry no token")

	def test_list_json_is_the_items_as_answered(self):
		out, _, _ = self.run_task("node", "list", "--json")
		self.assertEqual(json.loads(out), [self.state.held["nodes"]["tyo"]])

	def test_get_shows_the_spec_in_the_catalog_s_order_then_the_rest(self):
		out, _, _ = self.run_task("node", "get", "tyo")
		lines = out.splitlines()
		self.assertEqual(lines[0], "node tyo")
		self.assertIn(NODE_ID, lines[1], "get shows the id whole")
		names = [line.split()[0] for line in lines[3:]]
		self.assertEqual(names, ["address", "tier", "domain", "system", "retired", "roles", "grants"])
		self.assertIn("compute,core", out)

	def test_get_by_id_and_as_json(self):
		out, _, _ = self.run_task("node", "get", NODE_ID, "--json")
		self.assertEqual(json.loads(out)["name"], "tyo")

	def test_a_kind_the_catalog_does_not_hold_fails_naming_those_it_does(self):
		code = self.failed("widgets", "list")
		self.assertIn("node (nodes)", code)

	def test_a_resource_not_held_fails_with_the_code(self):
		code = self.failed("node", "get", "zzz")
		self.assertIn("no_such_resource", code)


class Writing(Command):
	def test_create_puts_at_revision_zero_as_operator_with_the_token(self):
		out, _, code = self.run_task("app", "create", "ledger", "--set", "placements=tyo,sin")
		self.assertIsNone(code, out)
		(sent,) = self.writes()
		self.assertEqual((sent["method"], sent["path"]), ("PUT", "/v1/apps/ledger"))
		self.assertEqual(sent["auth"], f"Bearer {TOKEN}")
		spec = {"placements": ["tyo", "sin"]}
		self.assertEqual(sent["body"], {"by": "operator", "revision": 0, "spec": spec})
		self.assertIn("written, revision 11, task 00000000000000f1", out)

	def test_create_resolves_a_reference_by_name_and_types_every_scalar(self):
		_, _, code = self.run_task(
			"node",
			"create",
			"sin",
			"--set",
			"address=100.64.0.2",
			"--set",
			"tier=datacenter",
			"--set",
			"domain=oci",
			"--set",
			"system=alpine",
			"--set",
			"expiry=2036",
			"--set",
			"core=true",
			"--json",
			'grants=["hub:peer"]',
		)
		self.assertIsNone(code)
		spec = self.writes()[0]["body"]["spec"]
		self.assertEqual(spec["domain"], DOMAIN_ID)
		self.assertEqual((spec["expiry"], spec["core"], spec["grants"]), (2036, True, ["hub:peer"]))

	def test_create_without_a_required_field_fails(self):
		code = self.failed("app", "create", "ledger")
		self.assertIn("placements is required", code)

	def test_create_of_an_id_shaped_name_fails(self):
		self.failed("domain", "create", "0123456789abcdef")

	def test_set_is_typed_before_anything_is_sent(self):
		for pair, said in [
			("expiry=soon", "integer"),
			("retired=yes", "true or false"),
			("tier=moon", "one of"),
			("roles=compute,gpu", "never gpu"),
			("roles=core,core", "once"),
			("release=x", "--json"),
			("color=red", "not a field"),
		]:
			with self.subTest(pair):
				self.assertIn(said, self.failed("node", "patch", "tyo", "--set", pair))

	def test_patch_reads_then_merges_at_the_revision_read(self):
		given = ("--set", "retired=true", "--unset", "roles")
		out, _, code = self.run_task("node", "patch", "tyo", *given)
		self.assertIsNone(code, out)
		(sent,) = self.writes()
		self.assertEqual((sent["method"], sent["path"]), ("PATCH", f"/v1/nodes/{NODE_ID}"))
		spec = {"retired": True, "roles": None}
		self.assertEqual(sent["body"], {"by": "operator", "revision": 10, "spec": spec})
		self.assertNotIn("roles", self.state.held["nodes"]["tyo"]["spec"])

	def test_a_required_field_is_never_unset(self):
		self.assertIn("required", self.failed("node", "patch", "tyo", "--unset", "address"))

	def test_a_write_that_changes_nothing_says_unchanged(self):
		out, _, code = self.run_task("node", "patch", "tyo", "--set", "retired=false")
		self.assertIsNone(code)
		self.assertIn("node tyo: unchanged", out)

	def test_a_refusal_is_printed_with_its_code_and_exits_1(self):
		self.state.refusal = ("forbidden_field", "`status` is not written")
		_, _, code = self.run_task("node", "patch", "tyo", "--set", "retired=true")
		self.assertIsInstance(code, str)
		self.assertIn("refused forbidden_field: `status` is not written", code)

	def test_a_write_without_the_token_sends_nothing(self):
		with mock.patch.dict(os.environ, {"REGISTRY_TOKEN": ""}):
			self.assertIn("REGISTRY_TOKEN", self.failed("domain", "create", "home"))

	def test_nothing_to_change_fails(self):
		self.assertIn("nothing to change", self.failed("node", "patch", "tyo"))


class Secrets(Command):
	def test_an_item_s_secret_is_asked_for_once_per_item_and_sent(self):
		items = ("--identity", "email", "--identity", "tenancy")
		out, _, code = self.run_task("account", "create", "az", "--set", "provider=azure", *items)
		self.assertIsNone(code, out)
		self.assertEqual(self.asked, ["identity email: value: ", "identity tenancy: value: "])
		identities = self.writes()[0]["body"]["spec"]["identities"]
		sent = [{"kind": kind, "value": SECRET_VALUE} for kind in ("email", "tenancy")]
		self.assertEqual(identities, sent)
		self.assertNotIn(SECRET_VALUE, out)

	def test_patch_replaces_the_item_of_its_key_and_keeps_the_rest(self):
		_, _, code = self.run_task("account", "patch", "oci-2", "--identity", "email")
		self.assertIsNone(code)
		identities = self.writes()[0]["body"]["spec"]["identities"]
		self.assertEqual([i["kind"] for i in identities], ["user_id", "email"])
		self.assertEqual(identities[0]["secret"], "accounts/registry/ACCOUNT_00000000000000A1_USER_ID")
		self.assertEqual(identities[1], {"kind": "email", "value": SECRET_VALUE})

	def test_a_secret_is_never_taken_from_an_argument(self):
		for arguments in [
			("--identity", f"email,value={SECRET_VALUE}"),
			("--json", f'identities=[{{"kind": "email", "value": "{SECRET_VALUE}"}}]'),
		]:
			with self.subTest(arguments[0]):
				code = self.failed("account", "patch", "oci-2", *arguments)
				self.assertIn("never taken from an argument", code)
				self.assertNotIn(SECRET_VALUE, code)
		self.assertEqual(self.asked, [])

	def test_what_the_registry_writes_is_not_given(self):
		code = self.failed("account", "patch", "oci-2", "--identity", "email,secret=a/b/C")
		self.assertIn("not editable", code)

	def test_an_item_key_is_one_of_its_choices(self):
		self.assertIn("one of", self.failed("account", "patch", "oci-2", "--identity", "password"))

	def test_a_dry_run_asks_for_nothing_sends_nothing_and_hides_every_secret(self):
		out, _, code = self.run_task(
			"account", "create", "az", "--set", "provider=azure", "--identity", "email", "--dry-run"
		)
		self.assertIsNone(code)
		self.assertEqual((self.asked, self.writes()), ([], []))
		self.assertIn("would PUT /v1/accounts/az", out)
		body = json.loads(out[out.index("{") :])
		self.assertEqual(body["spec"]["identities"], [{"kind": "email", "value": "<hidden>"}])
		self.assertNotIn(TOKEN, out)
		self.assertNotIn(SECRET_VALUE, out)

	def test_a_patch_dry_run_reads_the_revision_and_writes_nothing(self):
		out, _, _ = self.run_task("node", "patch", "tyo", "--set", "retired=true", "-n")
		self.assertEqual(self.writes(), [])
		self.assertIn(f"would PATCH /v1/nodes/{NODE_ID}", out)
		self.assertEqual(json.loads(out[out.index("{") :])["revision"], 10)


class Naming(unittest.TestCase):
	def test_a_kind_is_reached_at_its_plural_and_an_older_catalog_s_at_its_name_and_an_s(self):
		self.assertEqual(platform.plural_of({"name": "domain", "plural": "domains"}), "domains")
		self.assertEqual(platform.plural_of({"name": "node"}), "nodes")

	def test_an_item_is_named_by_its_list_s_singular(self):
		names = [platform.singular(n) for n in ("identities", "records", "data")]
		self.assertEqual(names, ["identity", "record", "data"])


if __name__ == "__main__":
	unittest.main()
