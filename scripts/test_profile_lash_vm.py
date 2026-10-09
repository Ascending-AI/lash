#!/usr/bin/env python3
"""Configured stack capacity is metadata, never a stack-occupancy verdict."""
import json
import unittest
from unittest.mock import patch

import profile_lash_vm
import perfreport


class ConfiguredStackCapacityTest(unittest.TestCase):
    def test_configured_stack_does_not_certify_usage(self):
        with patch.dict("os.environ", {"RUST_MIN_STACK": "4096"}), patch.object(
            profile_lash_vm, "process_stack_limits", return_value=(8192, None, True)
        ):
            profile = profile_lash_vm.current_stack_profile(2048)
        print(json.dumps(profile, sort_keys=True))
        self.assertEqual(profile.get("configured_stack_capacity_bytes"), 4096)
        self.assertEqual(profile.get("configured_stack_capacity_source"), "rust_min_stack")
        self.assertNotIn("within_stack_budget", profile)
        self.assertNotIn("measured_stack_bytes", profile)
        summary = perfreport.fmt_stack_profile(profile)
        self.assertIn("configured_capacity=4.00KiB", summary)
        self.assertIn("configured_budget=2.00KiB", summary)
        self.assertNotIn("within_budget", summary)

    def test_process_limit_is_configured_capacity_when_thread_default_is_unknown(self):
        with patch.dict("os.environ", {"RUST_MIN_STACK": "invalid"}), patch.object(
            profile_lash_vm, "process_stack_limits", return_value=(8192, None, True)
        ):
            profile = profile_lash_vm.current_stack_profile(2048)
        self.assertEqual(profile.get("configured_stack_capacity_bytes"), 8192)
        self.assertEqual(profile.get("configured_stack_capacity_source"), "process_stack_soft_limit")

    def test_unknown_capacity_stays_unknown(self):
        with patch.dict("os.environ", {"RUST_MIN_STACK": "invalid"}), patch.object(
            profile_lash_vm, "process_stack_limits", return_value=(None, None, True)
        ):
            profile = profile_lash_vm.current_stack_profile(2048)
        self.assertIsNone(profile["configured_stack_capacity_bytes"])
        self.assertIsNone(profile["configured_stack_capacity_source"])


if __name__ == "__main__":
    unittest.main()
