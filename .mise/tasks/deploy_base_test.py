"""deploy-base's choice of the commit a push plans against.

`python3 .mise/tasks/deploy_base_test.py`. Kept identical in monoflake/platform, monoflake/infra and
canmi21/cue, beside deploy-base in each, as deploy-base is.
"""

import importlib.machinery
import importlib.util
import json
import subprocess
import tempfile
import unittest
from pathlib import Path
from unittest import mock

HERE = Path(__file__).resolve().parent
loader = importlib.machinery.SourceFileLoader("task_deploy_base", str(HERE / "deploy-base"))
task = importlib.util.module_from_spec(importlib.util.spec_from_loader(loader.name, loader))
loader.exec_module(task)

BUILT = "1" * 40
BEFORE = "2" * 40
HEAD = "3" * 40
ZERO = "0" * 40


def answering(commit):
	return lambda: commit


def unanswered():
	raise task.Unanswered("HTTP 403")


class Base(unittest.TestCase):
	def test_a_push_plans_against_the_last_commit_built_not_its_before(self):
		# The cancelled pending run's commit is BEFORE; the last run that succeeded built BUILT.
		chosen, why = task.base(answering(BUILT), BEFORE, HEAD, lambda commit: True)
		self.assertEqual(chosen, BUILT)
		self.assertIn(BUILT, why)

	def test_with_no_run_built_everything_is_built(self):
		self.assertEqual(task.base(answering(None), BEFORE, HEAD, lambda commit: True)[0], "")

	def test_a_commit_head_does_not_descend_from_builds_everything(self):
		chosen, why = task.base(answering(BUILT), BEFORE, HEAD, lambda commit: False)
		self.assertEqual(chosen, "")
		self.assertIn("does not descend", why)

	def test_the_api_unanswered_falls_back_to_before_and_says_so(self):
		chosen, why = task.base(unanswered, BEFORE, HEAD, lambda commit: True)
		self.assertEqual(chosen, BEFORE)
		self.assertIn("HTTP 403", why)
		self.assertIn("github.event.before", why)
		self.assertEqual(task.base(unanswered, ZERO, HEAD, lambda commit: True)[0], "")
		self.assertEqual(task.base(unanswered, "", HEAD, lambda commit: True)[0], "")

	def test_a_rerun_of_the_commit_built_plans_against_its_before(self):
		self.assertEqual(task.base(answering(HEAD), BEFORE, HEAD, lambda commit: True)[0], BEFORE)


class LastBuilt(unittest.TestCase):
	def asked(self, returncode=0, stdout="", stderr="", raised=None):
		ran = mock.Mock(returncode=returncode, stdout=stdout, stderr=stderr)
		with mock.patch.object(task.subprocess, "run", return_value=ran, side_effect=raised) as run:
			try:
				return task.last_built("monoflake/platform"), run.call_args
			except task.Unanswered as error:
				return error, run.call_args

	def test_asks_for_the_newest_push_run_on_main_that_succeeded(self):
		runs = {"workflow_runs": [{"head_sha": BUILT, "event": "push"}]}
		answer, call = self.asked(stdout=json.dumps(runs))
		self.assertEqual(answer, BUILT)
		path = call.args[0][2]
		self.assertTrue(path.startswith("repos/monoflake/platform/actions/workflows/deploy.yml/runs?"))
		for asked in ["branch=main", "event=push", "status=success", "per_page=1"]:
			self.assertIn(asked, path)

	def test_none_succeeded_is_none(self):
		self.assertIsNone(self.asked(stdout='{"total_count": 0, "workflow_runs": []}')[0])

	def test_every_failure_is_unanswered(self):
		for kwargs in [
			{"returncode": 1, "stderr": "HTTP 403: Resource not accessible by integration\n"},
			{"stdout": "not json"},
			{"stdout": '{"message": "Not Found"}'},
			{"stdout": '{"workflow_runs": [{}]}'},
			{"raised": FileNotFoundError("gh")},
			{"raised": subprocess.TimeoutExpired("gh", 30)},
		]:
			self.assertIsInstance(self.asked(**kwargs)[0], task.Unanswered, kwargs)
		with self.assertRaises(task.Unanswered):
			task.last_built("")


class Holds(unittest.TestCase):
	"""Against a real repository: two commits on one line, and one on a line of its own."""

	def git(self, *arguments):
		run = ["git", "-c", "user.name=t", "-c", "user.email=t@t.invalid", *arguments]
		ran = subprocess.run(run, cwd=self.checkout, capture_output=True, text=True, check=True)
		return ran.stdout.strip()

	def setUp(self):
		made = tempfile.TemporaryDirectory()
		self.addCleanup(made.cleanup)
		self.checkout = made.name
		self.git("init", "-q", "-b", "main")
		self.git("commit", "-q", "--allow-empty", "-m", "first")
		self.first = self.git("rev-parse", "HEAD")
		self.git("checkout", "-q", "--orphan", "aside")
		self.git("commit", "-q", "--allow-empty", "-m", "aside")
		self.aside = self.git("rev-parse", "HEAD")
		self.git("checkout", "-q", "main")
		self.git("commit", "-q", "--allow-empty", "-m", "second")

	def test_head_descends_from_an_earlier_commit_and_its_own(self):
		self.assertTrue(task.holds(self.first, self.checkout))
		self.assertTrue(task.holds(self.git("rev-parse", "HEAD"), self.checkout))

	def test_a_commit_off_its_line_or_unknown_is_not_held(self):
		self.assertFalse(task.holds(self.aside, self.checkout))
		self.assertFalse(task.holds("4" * 40, self.checkout))


if __name__ == "__main__":
	unittest.main()
