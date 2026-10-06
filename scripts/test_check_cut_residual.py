#!/usr/bin/env python3
"""The cut allows generated reset outputs and rejects handwritten residual work."""

from pathlib import Path
import subprocess
import tempfile
import unittest
from unittest.mock import patch

import check_cut_residual as residual


class CutResidualTests(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.repo = Path(directory.name)
        self.git("init", "--quiet", "--initial-branch=main")
        self.plan = {
            "source_edits": ["source.rs"], "generated_paths": ["generated.json"],
            "fixture_paths": ["fixture.json"], "build_inventory_paths": ["BUCK"],
            "schema_renames": {"v100.schema.json": "v1.schema.json"},
        }
        for path in residual.reset_paths(self.plan) | {"handwritten.rs", ".github/workflows/ci.yml"}:
            target = self.repo / path
            target.parent.mkdir(parents=True, exist_ok=True)
            target.write_text("before\n")
        self.base = self.commit()

    def git(self, *args):
        return subprocess.check_output(
            ["git", "-C", str(self.repo), "-c", "user.name=cut", "-c", "user.email=cut@example.invalid", *args],
            text=True,
        ).strip()

    def commit(self):
        self.git("add", "--all")
        self.git("commit", "--quiet", "-m", "fixture")
        return self.git("rev-parse", "HEAD")

    def test_every_declared_reset_output_and_cargo_manifest_is_allowed(self):
        for path in residual.reset_paths(self.plan):
            (self.repo / path).write_text("after\n")
        self.assertEqual(residual.check(self.repo, self.base, self.commit(), self.plan)["unexpected_paths"], [])

    def test_handwritten_work_and_unlisted_generated_files_are_rejected(self):
        (self.repo / "handwritten.rs").write_text("after\n")
        (self.repo / "unlisted.json").write_text("after\n")
        self.assertEqual(residual.check(self.repo, self.base, self.commit(), self.plan)["unexpected_paths"],
                         ["handwritten.rs", "unlisted.json"])

    def test_activation_does_not_allow_workflow_logic_or_new_exemptions(self):
        before = "jobs:\n  version-bumps:\n    continue-on-error: true\n    run: check\n  publish:\n    needs: [build]\n"
        after = before.replace("continue-on-error: true", "continue-on-error: false").replace("[build]", "[build, version-bumps]")
        self.assertTrue(residual.activation_only(before, after))
        self.assertFalse(residual.activation_only(before, after.replace("run: check", "run: skip")))
        self.assertFalse(residual.activation_only(after, before))
        workflow = self.repo / ".github/workflows/ci.yml"
        workflow.write_text(before)
        base = self.commit()
        workflow.write_text(after)
        self.assertEqual(residual.check(self.repo, base, self.commit(), self.plan)["unexpected_paths"], [])

    def test_an_unrelated_job_cannot_change_its_exemption(self):
        before = "jobs:\n  other-job:\n    continue-on-error: true\n"
        self.assertFalse(residual.activation_only(before, before.replace("true", "false")))

    def test_the_reset_plan_comes_from_the_unreset_base(self):
        (self.repo / "source.rs").write_text("const VERSION: u32 = 1;\n")
        self.commit()
        def plan(tree):
            self.assertEqual((tree / "source.rs").read_text(), "before\n")
            return self.plan, {}
        with patch.object(residual.release_reset, "plan", side_effect=plan):
            self.assertEqual(residual.base_plan(self.repo, self.base), self.plan)

    def test_a_candidate_that_has_not_been_rebased_is_rejected(self):
        (self.repo / "source.rs").write_text("after\n")
        newer = self.commit()
        with self.assertRaises(subprocess.CalledProcessError):
            residual.check(self.repo, newer, self.base, self.plan)


if __name__ == "__main__":
    unittest.main()
