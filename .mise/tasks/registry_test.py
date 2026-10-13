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
		body = registry.request_body(NODE, "100.64.0.1", 5)
		self.assertEqual((body["by"], body["revision"]), ("import", 5))
		derived = {"hosting": "datacenter", "roles": ["compute", "core"]}
		self.assertEqual(body["spec"], {**NODE, **derived, "address": "100.64.0.1"})
		self.assertEqual(body["spec"]["grants"], ["hub:peer"])

	def test_the_ids_take_the_codes_place(self):
		body = registry.request_body(NODE, "100.64.0.1", 5, "00000000000000a1", "00000000000000d1")
		spec = body["spec"]
		self.assertEqual((spec["account"], spec["domain"]), ("00000000000000a1", "00000000000000d1"))
		home = registry.request_body(NODE, "100.64.0.1", 5, None, "00000000000000d2")["spec"]
		self.assertEqual((home["account"], home["domain"]), (None, "00000000000000d2"))

	def test_a_put_leaves_out_what_a_patch_takes_out(self):
		body = registry.request_body(NODE, "100.64.0.1", 0, None, "00000000000000d2")
		self.assertNotIn("account", registry.for_method(body, "PUT")["spec"])
		self.assertIsNone(registry.for_method(body, "PATCH")["spec"]["account"])
		self.assertIsNone(body["spec"]["account"], "the body given is not changed")

	def test_the_table_given_is_not_changed(self):
		before = dict(NODE)
		registry.request_body(NODE, "100.64.0.1", 0)
		self.assertEqual(NODE, before)

	def test_optional_keys_stay_absent_when_the_table_lacks_them(self):
		table = {"tier": "datacenter", "domain": "oci", "system": "alpine"}
		self.assertEqual(
			registry.request_body(table, "100.64.0.2", 0)["spec"],
			{
				"tier": "datacenter",
				"domain": "oci",
				"system": "alpine",
				"hosting": "datacenter",
				"roles": ["compute"],
				"address": "100.64.0.2",
			},
		)


class Derived(unittest.TestCase):
	"""RFC 32hd9's fields from the old axes: written beside them, the new from the old."""

	def test_home_is_selfhost_and_every_other_tier_datacenter(self):
		pairs = [("home", "selfhost"), ("datacenter", "datacenter"), ("transient", "datacenter")]
		for tier, hosting in pairs:
			self.assertEqual(registry.derived({"tier": tier, "domain": "oci"})["hosting"], hosting, tier)

	def test_a_provider_s_code_is_the_account_and_the_domain_and_a_home_its_own_domain(self):
		datacenter = {"tier": "datacenter", "domain": "oci-2"}
		self.assertEqual(registry.homed("sin", datacenter), ("oci-2", "oci-2"))
		self.assertEqual(registry.homed("rdu", {"tier": "home", "domain": "int"}), (None, "rdu"))

	def test_each_account_names_its_nodes_and_each_domain_is_named_once(self):
		nodes = {
			"tyo": {"tier": "datacenter", "domain": "oci"},
			"nrt": {"tier": "datacenter", "domain": "oci"},
			"sin": {"tier": "datacenter", "domain": "oci-2"},
			"gvx": {"tier": "datacenter", "domain": "az"},
			"rdu": {"tier": "home", "domain": "int"},
			"sha": {"tier": "home", "domain": "int"},
		}
		accounts, domains = registry.resources_named(nodes)
		self.assertEqual(accounts, {"oci": ["tyo", "nrt"], "oci-2": ["sin"], "az": ["gvx"]})
		self.assertEqual(domains, ["oci", "oci-2", "az", "rdu", "sha"])

	def test_every_node_computes_and_a_core_is_core_too(self):
		self.assertEqual(registry.derived({"core": True})["roles"], ["compute", "core"])
		for table in [{}, {"core": False}]:
			self.assertEqual(registry.derived(table)["roles"], ["compute"], table)

	def test_a_key_the_table_carries_is_the_table_s(self):
		table = {"tier": "home", "domain": "rkn", "hosting": "datacenter", "roles": ["storage"]}
		spec = registry.request_body(table, "100.64.0.3", 0)["spec"]
		self.assertEqual((spec["hosting"], spec["roles"]), ("datacenter", ["storage"]))


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

	def test_a_name_of_an_id_s_shape_fails(self):
		declarations = [("a", {"name": "0123456789abcdef", "placements": ["tyo"]})]
		with self.assertRaises(SystemExit):
			registry.apps_placed(declarations)
		self.assertTrue(registry.is_id("0123456789abcdef"))
		for name in ["web", "0123456789abcde", "0123456789ABCDEF", "0123456789abcdef0123456789abcdef"]:
			self.assertFalse(registry.is_id(name), name)

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

	def test_a_node_the_registry_does_not_hold_is_at_revision_zero(self):
		absent = {"status": "error", "code": "no_such_node", "message": "No node has this name"}
		self.assertEqual(registry.revision_of(absent, "sin", "node"), 0)
		with self.assertRaises(SystemExit):
			registry.revision_of(absent, "web")


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


class Fake:
	"""A registry answering as the registry does: each kind by its route, an id made by the `PUT`
	that first holds a resource, `PUT` setting a spec whole and `PATCH` the keys given, a key given
	`None` taken out, each held to the revision read."""

	def __init__(self):
		self.held = {}
		self.written = []
		self.bodies = {}
		self.revision = 100

	def hold(self, plural, name, spec, revision):
		made = f"{len(self.held) + 1:016x}"
		self.held[(plural, name)] = {"id": made, "revision": revision, "spec": spec}

	def id_of(self, plural, name):
		return self.held[(plural, name)]["id"]

	def spec_of(self, plural, name):
		return self.held[(plural, name)]["spec"]

	def __call__(self, addresses, method, path, body=None, secret=None):
		_, _, plural, name = path.split("/", 3)
		kind = plural.removesuffix("s")
		held = self.held.get((plural, name))
		if method == "GET":
			if held is None:
				return {"status": "error", "code": registry.ABSENT[kind]}
			return {"status": "success", "data": {"kind": kind, "name": name, **held}}
		self.written.append((method, plural, name))
		self.bodies[(method, plural, name)] = body
		if held is None and method == "PATCH":
			return {"status": "error", "code": registry.ABSENT[kind]}
		if body["revision"] != (held["revision"] if held else 0):
			return {"status": "error", "code": "invalid_revision"}
		spec = dict(held["spec"]) if method == "PATCH" else {}
		for key, value in body["spec"].items():
			if value is None:
				spec.pop(key, None)
			else:
				spec[key] = value
		self.revision += 1
		made = held["id"] if held else f"{self.revision:016x}"
		self.held[(plural, name)] = {"id": made, "revision": self.revision, "spec": spec}
		return {"status": "success", "data": {"revision": self.revision}}


class Importing(unittest.TestCase):
	"""`import` run whole over a workspace made for it: repos.toml, three checkouts on disk, and what
	each one's `main` holds -- infra's node task, and cue's declarations one level shallower than the
	others', as they are -- served by a jj stood in for. Every machine it would reach is stood in for
	too, so a name it shadows, a source it misses or a branch it breaks fails here rather than on the
	operator's terminal."""

	NODES = {
		"tyo": {**NODE},
		"sin": {"tier": "datacenter", "domain": "oci", "system": "debian"},
		"rdu": {"tier": "home", "domain": "int", "system": "debian", "core": True},
	}

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
		addresses = {"tyo": "100.64.0.1", "sin": "100.64.0.2", "rdu": "100.64.0.3"}
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
			# A dry run reads which accounts are held; an empty registry, unless a test gives one.
			answered = answered if answered is not None else Fake()
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
		fake = Fake()
		identities = [{"kind": "email", "secret": "platform/registry/OCI_EMAIL"}]
		fake.hold("accounts", "oci", {"provider": "oracle", "identities": identities}, 7)
		lines = self.run_import(True, fake)
		self.assertEqual(fake.written, [], "a dry run reads, and writes nothing")
		puts = [line.split()[1] for line in lines if line.startswith("  PATCH /v1/apps/")]
		self.assertEqual(puts, ["/v1/apps/caddy", "/v1/apps/web", "/v1/apps/hub"])
		web = registry.app_body("<read>", ["tyo", "sin"])
		self.assertIn(f"  PATCH /v1/apps/web {registry.json.dumps(web)}", lines)
		self.assertIn("  skip aka", lines)
		self.assertEqual(sum(line.startswith("  PATCH /v1/nodes/") for line in lines), 3)
		ids = ("<the id of account oci>", "<the id of domain oci>")
		tyo = registry.request_body(self.NODES["tyo"], "100.64.0.1", "<read>", *ids)
		self.assertIn(f"  PATCH /v1/nodes/tyo {registry.json.dumps(tyo)}", lines)
		made = [line.split()[1] for line in lines if line.startswith("  PUT /v1/")]
		self.assertEqual(made, ["/v1/domains/oci", "/v1/domains/rdu"])
		self.assertIn("registry: account oci: linked, tyo, sin", lines)
		self.assertNotIn("s3cret", "\n".join(lines))

	def test_a_dry_run_says_an_account_not_held_and_the_nodes_left_without_one(self):
		lines = self.run_import(True)
		self.assertIn(
			"registry: account oci: not held; tyo, sin have none until it is written with its identities",
			lines,
		)
		domain = "<the id of domain oci>"
		sin = registry.request_body(self.NODES["sin"], "100.64.0.2", "<read>", None, domain)
		self.assertIn(f"  PATCH /v1/nodes/sin {registry.json.dumps(sin)}", lines)

	def test_a_run_makes_the_domains_then_writes_at_the_revision_it_read(self):
		fake = Fake()
		# What the import before this one left: the code as the account.
		fake.hold("nodes", "tyo", {"address": "100.64.0.9", "account": "oci"}, 3)
		lines = self.run_import(False, fake)
		self.assertEqual(
			fake.written[:5],
			[
				("PUT", "domains", "oci"),
				("PUT", "domains", "rdu"),
				("PATCH", "nodes", "tyo"),
				("PUT", "nodes", "sin"),
				("PUT", "nodes", "rdu"),
			],
		)
		accounts = [entry for entry in fake.written if entry[1] == "accounts"]
		self.assertFalse(accounts, "the import never makes an account")
		self.assertEqual(fake.bodies[("PATCH", "nodes", "tyo")]["revision"], 3)
		domain = fake.id_of("domains", "oci")
		for node in ["tyo", "sin"]:
			spec = fake.spec_of("nodes", node)
			self.assertEqual(spec["domain"], domain, node)
			self.assertNotIn("account", spec, f"{node}: no account until the author writes it")
		self.assertIn("registry: domain oci: made, revision 101", lines)
		self.assertIn(
			"registry: account oci: not held; tyo, sin have none until it is written with its identities",
			lines,
		)
		self.assertIn("registry: app aka: skipped, not placed on nodes", lines)
		web = fake.bodies[("PUT", "apps", "web")]
		self.assertEqual(web, registry.app_body(0, ["tyo", "sin"]))

	def test_a_home_node_has_no_account_and_its_own_domain(self):
		fake = Fake()
		# What the import before this one left: the code of its house as the account.
		left = {"tier": "home", "domain": "int", "account": "int", "address": "100.64.0.3"}
		fake.hold("nodes", "rdu", left, 4)
		self.run_import(False, fake)
		rdu = fake.spec_of("nodes", "rdu")
		self.assertNotIn("account", rdu)
		self.assertEqual((rdu["domain"], rdu["hosting"]), (fake.id_of("domains", "rdu"), "selfhost"))
		self.assertNotIn(("accounts", "int"), fake.held)
		self.assertNotIn(("domains", "int"), fake.held)

	def test_an_account_the_author_wrote_is_linked_and_never_written(self):
		fake = Fake()
		identities = [{"kind": "email", "secret": "platform/registry/OCI_EMAIL"}]
		fake.hold("accounts", "oci", {"provider": "oracle", "identities": identities}, 7)
		lines = self.run_import(False, fake)
		self.assertFalse([entry for entry in fake.written if entry[1] == "accounts"])
		self.assertEqual(fake.spec_of("accounts", "oci")["identities"], identities)
		self.assertIn("registry: account oci: linked, tyo, sin", lines)
		for node in ["tyo", "sin"]:
			self.assertEqual(fake.spec_of("nodes", node)["account"], fake.id_of("accounts", "oci"))
		self.assertNotIn("account", fake.spec_of("nodes", "rdu"))

	def test_a_rerun_keeps_the_release_an_app_already_has(self):
		release = {"repository": "monoflake/platform", "run": 7, "commit": "0" * 40}
		fake = Fake()
		fake.hold("apps", "web", {"placements": ["tyo"], "release": release}, 4)
		self.run_import(False, fake)
		web = fake.spec_of("apps", "web")
		self.assertEqual(web, {"placements": ["tyo", "sin"], "release": release})
		apps = [entry for entry in fake.written if entry[1] == "apps"]
		puts = [("PUT", "apps", "caddy"), ("PATCH", "apps", "web"), ("PUT", "apps", "hub")]
		self.assertEqual(apps, puts)

	def test_a_rerun_keeps_a_node_retired_in_the_registry(self):
		"""An operator retired tyo in the registry, and nodes.toml says nothing of it; the import
		sets what nodes.toml carries, the ids in place of the codes, and keeps the rest."""
		fake = Fake()
		fake.hold("nodes", "tyo", {**self.NODES["tyo"], "address": "100.64.0.9", "retired": True}, 5)
		self.run_import(False, fake)
		tyo = {**registry.derived(self.NODES["tyo"]), **self.NODES["tyo"]}
		tyo.update(address="100.64.0.1", retired=True, domain=fake.id_of("domains", "oci"))
		self.assertEqual(fake.spec_of("nodes", "tyo"), tyo)

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
	GITHUB = "github-never-printed"
	KEY = {"ARTIFACTS_READ_KEY_ID": "key-id-never-printed", "ARTIFACTS_READ_SECRET": "key-never-printed"}
	SOURCES = ["monoflake/infra", "monoflake/platform", "canmi21/cue"]

	def run_token(
		self, dry, current="monoflake/platform canmi21/web", github=GITHUB, worker=True, key=KEY
	):
		self.written = []
		self.asked = []
		self.wrangled = []

		def wrangler(arguments, stdin=None):
			self.wrangled.append((arguments, stdin))
			listing = arguments[:2] == ["deployments", "list"]
			failed = listing and not worker
			said = "A request to the Cloudflare API failed. workers.api.error.script_not_found"
			return mock.Mock(returncode=1 if failed else 0, stdout="[]", stderr=said if failed else "")

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
			stack.enter_context(mock.patch.object(registry, "wrangler", wrangler))
			stack.enter_context(mock.patch.object(registry, "CONSOLE_DIRECTORY", HERE))
			if github is not None:
				stack.enter_context(mock.patch.dict(os.environ, {registry.GITHUB: github}))
			stack.enter_context(mock.patch.dict(os.environ, key))
			stack.enter_context(contextlib.redirect_stdout(printed))
			registry.token(dry)
		return printed.getvalue()

	def test_a_dry_run_says_every_write_asks_nothing_and_prints_no_secret(self):
		printed = self.run_token(True)
		self.assertEqual(self.written, [])
		self.assertEqual(self.asked, [])
		self.assertEqual(self.wrangled, [], "a dry run asks wrangler nothing")
		console = (
			"registry: console: would check the Worker `console` exists, `wrangler deployments list"
			f" --name console`, then run in {HERE}: wrangler secret put REGISTRY_TOKEN --name console,"
			" on stdin: REGISTRY_TOKEN=<redacted>"
		)
		self.assertIn(console, printed.splitlines())
		self.assertNotIn(self.SECRET, printed)
		self.assertNotIn(self.GITHUB, printed)
		for held in self.KEY.values():
			self.assertNotIn(held, printed)
		for line in [
			"registry: tyo: would set, keeping every other line, mode 600, in"
			" /data/apps/registry/secret.env: REGISTRY_TOKEN=<redacted>",
			"registry: sin: would set, keeping every other line, mode 600, in"
			" /data/apps/registry/secret.env: REGISTRY_TOKEN=<redacted>",
			"registry: tyo: would set in /data/apps/deployer/secret.env: REGISTRY_TOKEN=<redacted>",
			"registry: tyo: would set in /data/apps/deployer/secret.env:"
			" GITHUB_ACTIONS_TOKEN=<redacted>",
			"registry: tyo: would set in /data/apps/deployer/config.env:"
			" REGISTRY=100.64.0.1 100.64.0.2",
			"registry: tyo: would set in /data/apps/deployer/config.env: DEPLOY_SOURCES=<what it"
			" names>, then each of monoflake/infra monoflake/platform canmi21/cue it does not",
			"registry: tyo: would set in /data/apps/deployer/secret.env:"
			" ARTIFACTS_READ_KEY_ID=<redacted>",
			"registry: tyo: would set in /data/apps/deployer/secret.env:"
			" ARTIFACTS_READ_SECRET=<redacted>",
			*(
				f"registry: tyo: would set in /data/apps/deployer/config.env: {name}={told}"
				for name, told in registry.ARTIFACTS.items()
			),
		]:
			self.assertIn(line, printed.splitlines())

	def test_a_run_gives_the_registry_and_the_deployer_the_token_and_widens_the_sources(self):
		printed = self.run_token(False)
		self.assertNotIn(self.SECRET, printed)
		self.assertNotIn(self.GITHUB, printed)
		token = {"REGISTRY_TOKEN": self.SECRET}
		self.assertEqual(
			self.written,
			[
				("tyo", "registry", "secret.env", token),
				("sin", "registry", "secret.env", token),
				(
					"tyo",
					"deployer",
					"secret.env",
					{**token, "GITHUB_ACTIONS_TOKEN": self.GITHUB, **self.KEY},
				),
				(
					"tyo",
					"deployer",
					"config.env",
					{
						"REGISTRY": "100.64.0.1 100.64.0.2",
						"DEPLOY_SOURCES": "monoflake/platform canmi21/web monoflake/infra canmi21/cue",
						**registry.ARTIFACTS,
					},
				),
			],
		)
		[(node, command)] = self.asked
		self.assertEqual(node, "tyo")
		self.assertIn("/data/apps/deployer/config.env", command)
		self.assertIn("s/^DEPLOY_SOURCES=//p", command)

	def test_the_deployer_is_given_the_platforms_github_token_and_no_owners_own(self):
		self.run_token(False)
		[(_, _, _, pairs)] = [write for write in self.written if write[1:3] == ("deployer", "secret.env")]
		self.assertEqual(
			set(pairs),
			{"REGISTRY_TOKEN", "GITHUB_ACTIONS_TOKEN", "ARTIFACTS_READ_KEY_ID", "ARTIFACTS_READ_SECRET"},
		)

	def test_the_bucket_is_named_in_config_env_and_its_key_kept_in_secret_env(self):
		self.run_token(False)
		config = [pairs for _, app, file, pairs in self.written if (app, file) == ("deployer", "config.env")]
		self.assertEqual(
			{name: config[0][name] for name in registry.ARTIFACTS},
			{
				"ARTIFACTS_ENDPOINT": registry.ARTIFACTS["ARTIFACTS_ENDPOINT"],
				"ARTIFACTS_REGION": "us-west-004",
				"ARTIFACTS_BUCKET": "build-artifact",
			},
		)
		self.assertTrue(registry.ARTIFACTS["ARTIFACTS_ENDPOINT"].startswith("https://"))
		self.assertTrue(set(registry.ARTIFACTS_KEY).isdisjoint(config[0]))

	def test_a_writer_that_reads_no_artifacts_is_given_no_bucket(self):
		with mock.patch.object(registry, "ARTIFACT_READERS", ()):
			self.run_token(False)
		for _, app, _, pairs in self.written:
			self.assertTrue({*registry.ARTIFACTS, *registry.ARTIFACTS_KEY}.isdisjoint(pairs), app)

	def test_no_read_key_in_secrets_json_writes_nothing_to_a_writer(self):
		with mock.patch.dict(os.environ, {}, clear=False):
			os.environ.pop("ARTIFACTS_READ_SECRET", None)
			with self.assertRaises(SystemExit) as failed:
				self.run_token(False, key={"ARTIFACTS_READ_KEY_ID": "key-id-never-printed"})
		said = str(failed.exception.code)
		self.assertIn("secrets.json has no ARTIFACTS_READ_SECRET", said)
		self.assertNotIn("key-id-never-printed", said)
		self.assertEqual([app for _, app, _, _ in self.written], ["registry", "registry"])

	def test_no_github_token_in_secrets_json_writes_nothing_to_a_writer(self):
		with mock.patch.dict(os.environ, {}, clear=False):
			os.environ.pop(registry.GITHUB, None)
			with self.assertRaises(SystemExit) as failed:
				self.run_token(False, github=None)
		self.assertIn("secrets.json has no GITHUB_ACTIONS_TOKEN", str(failed.exception.code))
		self.assertEqual([app for _, app, _, _ in self.written], ["registry", "registry"])

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


	def test_the_console_worker_is_found_first_then_given_the_token_on_stdin(self):
		printed = self.run_token(False)
		self.assertEqual(
			self.wrangled,
			[
				(["deployments", "list", "--name", "console", "--json"], None),
				(["secret", "put", "REGISTRY_TOKEN", "--name", "console"], self.SECRET),
			],
		)
		self.assertNotIn(self.SECRET, printed)
		put = "registry: console: REGISTRY_TOKEN put as a Worker secret, read by its next request"
		self.assertIn(put, printed)

	def test_no_console_worker_writes_nothing_anywhere(self):
		with self.assertRaises(SystemExit) as failed:
			self.run_token(False, worker=False)
		self.assertIn("the Worker `console` is not there", str(failed.exception.code))
		self.assertEqual(self.written, [], "no node is written")
		self.assertEqual(len(self.wrangled), 1, "never `secret put`, which would make the Worker")


if __name__ == "__main__":
	unittest.main()
