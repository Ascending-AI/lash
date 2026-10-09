#!/usr/bin/env python3
"""Keep AppendVec's unsafe tests in the local Miri gate."""

from pathlib import Path
import os
import subprocess
import unittest


ROOT = Path(__file__).resolve().parents[1]


class AppendVecMiriTests(unittest.TestCase):
    def test_driver_reaches_miri_without_requiring_buck2(self):
        result = subprocess.run(["bash", "scripts/hermetic-build.sh", "miri", "--help"], cwd=ROOT, env=os.environ | {"BUCK2": "append-vec-miri-buck2-must-not-be-used"}, capture_output=True, text=True)
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("AppendVec", result.stdout)


if __name__ == "__main__":
    unittest.main()
