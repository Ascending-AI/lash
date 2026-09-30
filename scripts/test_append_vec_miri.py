#!/usr/bin/env python3
"""Keep AppendVec's unsafe tests in the scheduled full Miri gate."""

from pathlib import Path
import os
import subprocess
import tomllib
import unittest

import yaml

ROOT = Path(__file__).resolve().parents[1]


class AppendVecMiriTests(unittest.TestCase):
    def test_pinned_nightly_installs_miri_and_runs_all_append_vec_tests(self):
        pin = tomllib.loads((ROOT / "scripts/miri-toolchain.toml").read_text())["toolchain"]
        self.assertRegex(pin["channel"], r"^nightly-\d{4}-\d{2}-\d{2}$")
        self.assertIn("miri", pin["components"])
        self.assertIn("rust-src", pin["components"])
        script = (ROOT / "scripts/append-vec-miri.sh").read_text()
        self.assertIn("scripts/miri-toolchain.toml", script)
        self.assertIn("-Zmiri-many-seeds=0..20", script)
        self.assertIn("append_vec::tests::", script)
        self.assertIn("--test-threads=1", script)
        self.assertNotIn("disable-stacked-borrows", script)
        self.assertNotIn("disable-tree-borrows", script)
        self.assertNotIn("ignore-leaks", script)

    def test_scheduled_full_gate_uses_driver_and_is_required_by_conclusion(self):
        import ci_plan
        workflow = yaml.safe_load((ROOT / ".github/workflows/confidence.yml").read_text())
        job = workflow["jobs"]["append-vec-miri"]
        self.assertEqual("${{ github.event_name != 'workflow_dispatch' || inputs.lane == 'full' }}", job["if"])
        self.assertTrue(workflow[True]["schedule"])
        self.assertEqual(["bash scripts/hermetic-build.sh miri"], [step["run"] for step in job["steps"] if "miri" in step.get("run", "")])
        self.assertIn("append-vec-miri", workflow["jobs"]["confidence-conclusion"]["needs"])
        self.assertEqual("full-independent", ci_plan.CONFIDENCE_JOB_POLICY["append-vec-miri"])
        needs = {name: {"result": "skipped" if policy == "selector" else "success"} for name, policy in ci_plan.CONFIDENCE_JOB_POLICY.items()}
        self.assertEqual([], ci_plan.evaluate_confidence_conclusion(needs, "schedule", "full"))
        needs["append-vec-miri"]["result"] = "failure"
        self.assertTrue(ci_plan.evaluate_confidence_conclusion(needs, "schedule", "full"))
        self.assertNotIn("append-vec-miri", yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text())["jobs"])

    def test_driver_reaches_miri_without_requiring_bazel(self):
        result = subprocess.run(["bash", "scripts/hermetic-build.sh", "miri", "--help"], cwd=ROOT, env=os.environ | {"BAZEL": "append-vec-miri-bazel-must-not-be-used"}, capture_output=True, text=True)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("AppendVec", result.stdout)


if __name__ == "__main__":
    unittest.main()
