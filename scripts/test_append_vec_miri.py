#!/usr/bin/env python3
"""Keep AppendVec's unsafe tests in the local Miri gate."""

from pathlib import Path
import os
import subprocess
import tomllib
import unittest


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


    def test_driver_reaches_miri_without_requiring_buck2(self):
        result = subprocess.run(["bash", "scripts/hermetic-build.sh", "miri", "--help"], cwd=ROOT, env=os.environ | {"BUCK2": "append-vec-miri-buck2-must-not-be-used"}, capture_output=True, text=True)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("AppendVec", result.stdout)


if __name__ == "__main__":
    unittest.main()
