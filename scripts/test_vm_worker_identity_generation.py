#!/usr/bin/env python3
"""The build inventory must never own the worker's compiled fingerprint."""
import pathlib
import sys
import unittest
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/bazel"))
import generate_build_files as generator


class WorkerIdentityGenerationTests(unittest.TestCase):
    def test_inventory_does_not_generate_worker_identity(self):
        outputs = {}
        with patch.object(sys, "argv", ["generate_build_files.py", "--check"]), \
             patch.object(generator, "cargo_metadata", return_value={}), \
             patch.object(generator, "generated", return_value=(outputs, {})), \
             patch.object(generator, "reconcile_lane_units", return_value=[]), \
             patch.object(generator, "check", return_value=0):
            outputs[ROOT / "tools/bazel/target-inventory.json"] = '{"feature_lane_units": []}'
            self.assertEqual(generator.main(), 0)
        self.assertNotIn(ROOT / "crates/lash-vm-worker/src/identity.rs", outputs)

    def test_worker_identity_is_not_a_source_tree_file(self):
        self.assertFalse((ROOT / "crates/lash-vm-worker/src/identity.rs").exists())


if __name__ == "__main__":
    unittest.main()
