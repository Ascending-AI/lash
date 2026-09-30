#!/usr/bin/env python3
import unittest
from check_worker_wire_schema import check


class WorkerSchemaVersionLaws(unittest.TestCase):
    def test_pre_1_0_refreshes_in_place(self):
        self.assertEqual(check("0.9.0", 1, 1, {"wire-v1.snap": "old"}, {"wire-v1.snap": "new"}), [])

    def test_after_1_0_refresh_needs_a_bump_and_preserves_released_shapes(self):
        errors = check("1.0.0", 1, 1, {"wire-v1.snap": "old"}, {"wire-v1.snap": "new"})
        self.assertTrue(any("without a WORKER_PROTOCOL_VERSION bump" in error for error in errors))
        self.assertTrue(any("remain unchanged" in error for error in errors))

    def test_after_1_0_new_version_preserves_both_feature_snapshots(self):
        old = {"wire-v1.snap": "old", "wire-v1-testing.snap": "old testing"}
        new = old | {"wire-v2.snap": "new", "wire-v2-testing.snap": "new testing"}
        self.assertEqual(check("1.1.0", 1, 2, old, new), [])
        del new["wire-v2-testing.snap"]
        self.assertTrue(any("both base and testing" in error for error in check("1.1.0", 1, 2, old, new)))


if __name__ == "__main__":
    unittest.main()
