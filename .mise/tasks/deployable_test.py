"""deployable's choice of images, and its split between images to build and declarations to apply
alone, against this repository's own apps: `python3 .mise/tasks/deployable_test.py`."""

import importlib.machinery
import importlib.util
import sys
import unittest
from pathlib import Path

HERE = Path(__file__).resolve().parent
sys.path.insert(0, str(HERE))
loader = importlib.machinery.SourceFileLoader("deployable", str(HERE / "deployable"))
spec = importlib.util.spec_from_loader("deployable", loader)
deployable = importlib.util.module_from_spec(spec)
loader.exec_module(deployable)

DECLARED = deployable.apps()
GEO = "apps/compute/geo/src/main.rs"


class Revision:
	"""A revision holding `texts`, path to text, and the working copy's text of any other path."""

	def __init__(self, texts):
		self.texts = texts

	def read(self, path):
		if path in self.texts:
			return self.texts[path]
		file = deployable.ROOT / path
		return file.read_text() if file.exists() else None


def rebuilt(path, change=None):
	"""The images a change to `path` needs, by `change`, a function of its text; given none, with
	no revisions to read, as `--files` asks."""
	if change is None:
		return deployable.needed([path], DECLARED)
	before = (deployable.ROOT / path).read_text()
	revisions = (Revision({path: before}), Revision({path: change(before)}))
	return deployable.needed([path], DECLARED, revisions)


def split(path, change=None):
	"""The images a change to `path` needs rebuilt, and the declarations it leaves to apply alone."""
	if change is None:
		revisions = None
	else:
		before = (deployable.ROOT / path).read_text()
		revisions = (Revision({path: before}), Revision({path: change(before)}))
	images = deployable.needed([path], DECLARED, revisions)
	return images, deployable.redeclared([path], DECLARED, images, revisions)


class Declarations(unittest.TestCase):
	def test_a_changed_declaration_alone_is_applied_without_an_image(self):
		self.assertEqual(split("apps/compute/geo/service.toml"), ([], ["geo"]))

	def test_a_declaration_beside_anything_else_of_its_app_is_built(self):
		files = ["apps/compute/geo/service.toml", "apps/compute/geo/Dockerfile"]
		images = deployable.needed(files, DECLARED)
		self.assertEqual((images, deployable.redeclared(files, DECLARED, images)), (["geo"], []))

	def test_a_declaration_of_an_app_built_whole_is_applied_alone_too(self):
		# deployer's Dockerfile copies its whole directory; its declaration is still host's to read.
		self.assertEqual(split("apps/system/deployer/service.toml"), ([], ["deployer"]))

	def test_only_the_apps_own_declaration_is_one(self):
		self.assertTrue(deployable.declaration("apps/compute/geo/service.toml"))
		self.assertFalse(deployable.declaration("apps/compute/geo/src/service.toml"))
		self.assertFalse(deployable.declaration("apps/compute/geo/Dockerfile"))

	def test_a_comment_in_a_declaration_is_neither(self):
		self.assertEqual(split("apps/compute/geo/service.toml", lambda text: "# Why.\n" + text), ([], []))


class Fingerprints(unittest.TestCase):
	def test_a_comment_in_rust_is_no_change(self):
		self.assertEqual(rebuilt(GEO, lambda text: "// A note.\n" + text.replace("\n", "\n/* and */\n", 3)), [])

	def test_a_reformat_is_no_change(self):
		self.assertEqual(rebuilt(GEO, lambda text: text.replace("\t", "    ").replace(" {\n", "\n{\n")), [])

	def test_a_changed_token_is_a_rebuild(self):
		self.assertEqual(rebuilt(GEO, lambda text: text + "\nfn another() {}\n"), ["geo"])
		self.assertEqual(rebuilt(GEO), ["geo"])

	def test_prose_and_tests_build_nothing(self):
		for path in ("apps/compute/geo/README.md", "apps/data/primary/tests/proxy.rs", "LICENSE"):
			self.assertEqual(rebuilt(path), [], path)

	def test_a_value_in_a_declaration_is_applied_without_a_rebuild(self):
		declaration = "apps/compute/geo/service.toml"
		self.assertEqual(
			split(declaration, lambda text: text.replace('name = "geo"', 'name = "geo"\nhealth_timeout = 9')),
			([], ["geo"]),
		)

	def test_a_comment_in_a_dockerfile_is_no_change(self):
		dockerfile = "apps/compute/geo/Dockerfile"
		self.assertEqual(rebuilt(dockerfile, lambda text: text + "\n# Done.\n"), [])
		self.assertEqual(rebuilt(dockerfile, lambda text: text.replace("FROM ", "FROM --platform=$BUILDPLATFORM ", 1)), ["geo"])

	def test_the_build_is_every_image_unless_only_its_comments_moved(self):
		self.assertEqual(rebuilt(".dockerignore", lambda text: text + "# More.\n"), [])
		self.assertEqual(rebuilt(".dockerignore", lambda text: text + "**/cache\n"), sorted(DECLARED))

	def test_a_binarys_directory_holds_nothing_else_its_image_is_built_from(self):
		# apt's units and apk's door are installed on the node, never built into the image.
		for path in ("apps/system/apt/units/apt-nightly-update.service", "apps/system/apk/door/apk-door.sh"):
			self.assertEqual(rebuilt(path), [], path)
		# What a binary includes is in it.
		self.assertEqual(rebuilt("apps/observe/probe/checks.toml"), ["probe"])
		# deployer copies its whole directory, so anything there is its image's.
		self.assertNotIn("deployer", deployable.cargo_built(DECLARED, deployable.rust_graph()[0]))
		self.assertEqual(rebuilt("apps/system/deployer/src/extra.ts"), ["deployer"])


if __name__ == "__main__":
	unittest.main()
