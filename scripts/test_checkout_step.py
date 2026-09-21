#!/usr/bin/env python3
"""Every workflow checkout is the one script in scripts/ci/checkout-step.sh.

A GitHub-hosted job starts with an empty working directory, so the checkout
cannot be a local composite action: `uses: ./.github/actions/...` needs the
repository to already be present. The step is therefore inlined into every
job, and this test is what keeps the copies from drifting. Three steps are
deliberately different and listed here: the Lint job fetches every branch and
tag unshallowed, the hygiene job deepens both sides of the scanned range
(scripts/test_ci_plan_hygiene.py executes that text), and release.yml persists
its credentials through a config include so a later `git push` can use them.
"""

from __future__ import annotations

import os
import pathlib
import subprocess
import tempfile
import unittest

import yaml

ROOT = pathlib.Path(__file__).resolve().parents[1]
CANONICAL = ROOT / "scripts" / "ci" / "checkout-step.sh"
WORKFLOWS = ROOT / ".github" / "workflows"
EXEMPT = {("ci.yml", "lint"), ("ci.yml", "hygiene")}
EXEMPT_WORKFLOWS = {"release.yml"}
ALLOWED_ENV = {"CHECKOUT_TOKEN", "CHECKOUT_TAGS"}


def checkout_steps():
    for workflow in sorted(WORKFLOWS.glob("*.yml")):
        if workflow.name in EXEMPT_WORKFLOWS:
            continue
        jobs = yaml.safe_load(workflow.read_text(encoding="utf-8"))["jobs"]
        for job_id, job in jobs.items():
            if (workflow.name, job_id) in EXEMPT:
                continue
            for step in job.get("steps", []):
                if step.get("name") == "Check out repository":
                    yield workflow.name, job_id, step


class CheckoutStepContract(unittest.TestCase):
    def test_every_checkout_is_the_canonical_script(self) -> None:
        canonical = CANONICAL.read_text(encoding="utf-8")
        seen = 0
        for workflow, job_id, step in checkout_steps():
            with self.subTest(workflow=workflow, job=job_id):
                self.assertNotIn("uses", step, "a local action cannot check itself out")
                self.assertEqual(canonical, step["run"])
                env = step.get("env", {})
                self.assertEqual("${{ github.token }}", env.get("CHECKOUT_TOKEN"))
                self.assertLessEqual(set(env), ALLOWED_ENV)
                if "CHECKOUT_TAGS" in env:
                    self.assertEqual("--tags", env["CHECKOUT_TAGS"])
            seen += 1
        self.assertGreater(seen, 30, "the sweep found almost nothing; is the step renamed?")

    def test_exempt_jobs_still_exist(self) -> None:
        jobs = yaml.safe_load((WORKFLOWS / "ci.yml").read_text(encoding="utf-8"))["jobs"]
        for _, job_id in EXEMPT:
            self.assertIn(job_id, jobs)


class CheckoutStepBehaviour(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        root = pathlib.Path(self.temp.name)
        self.remote = root / "repo.git"
        self.remote.mkdir()
        git = self.remote_git
        git("init", "-q")
        git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "one")
        git("tag", "v1")
        git("-c", "user.name=t", "-c", "user.email=t@t", "commit", "-q", "--allow-empty", "-m", "two")
        self.sha = git("rev-parse", "HEAD")
        self.server_url = root.as_uri()

    def remote_git(self, *args: str) -> str:
        return subprocess.check_output(["git", "-C", str(self.remote), *args], text=True).strip()

    def run_checkout(self, sha: str, **extra: str) -> tuple[subprocess.CompletedProcess, pathlib.Path]:
        clone = pathlib.Path(tempfile.mkdtemp(dir=self.temp.name))
        env = dict(os.environ, GITHUB_SERVER_URL=self.server_url, GITHUB_REPOSITORY="repo",
                   GITHUB_SHA=sha, CHECKOUT_TOKEN="fixture-token", **extra)
        result = subprocess.run(["bash", "-c", CANONICAL.read_text(encoding="utf-8")], cwd=clone,
                                env=env, capture_output=True, text=True)
        return result, clone

    def test_shallow_checkout_of_the_trigger_sha(self) -> None:
        result, clone = self.run_checkout(self.sha)
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        git = lambda *a: subprocess.check_output(["git", "-C", str(clone), *a], text=True).strip()
        self.assertEqual(self.sha, git("rev-parse", "HEAD"))
        self.assertEqual("true", git("rev-parse", "--is-shallow-repository"))
        self.assertEqual("", git("tag"))
        self.assertEqual("0", git("config", "gc.auto"))

    def test_tags_are_fetched_on_request(self) -> None:
        result, clone = self.run_checkout(self.sha, CHECKOUT_TAGS="--tags")
        self.assertEqual(0, result.returncode, result.stdout + result.stderr)
        tags = subprocess.check_output(["git", "-C", str(clone), "tag"], text=True).split()
        self.assertEqual(["v1"], tags)

    def test_a_failing_fetch_is_retried_then_fails(self) -> None:
        result, _ = self.run_checkout("0" * 40)
        self.assertEqual(1, result.returncode)
        self.assertEqual(2, result.stdout.count("::warning::git fetch attempt"))


if __name__ == "__main__":
    unittest.main()
