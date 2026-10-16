#!/usr/bin/env python3
import unittest
from check_worker_wire_schema import SHAPES, check


class WorkerSchemaVersionLaws(unittest.TestCase):
    def test_pre_1_0_refreshes_in_place(self):
        self.assertEqual(check("0.9.0", 1, 1, {"worker-start/v1.schema.json": "old"}, {"worker-start/v1.schema.json": "new"}), [])

    def test_after_1_0_refresh_needs_a_bump_and_preserves_released_shapes(self):
        errors = check("1.0.0", 1, 1, {"worker-start/v1.schema.json": "old"}, {"worker-start/v1.schema.json": "new"})
        self.assertTrue(any("without a WORKER_PROTOCOL_VERSION bump" in error for error in errors))
        self.assertTrue(any("remain unchanged" in error for error in errors))

    def test_after_1_0_new_version_preserves_all_page_and_carry_schemas(self):
        old = {f"{shape}/v1.schema.json": f"old {shape}" for shape in SHAPES}
        new = old | {f"{shape}/v2.schema.json": f"new {shape}" for shape in SHAPES}
        self.assertEqual(check("1.1.0", 1, 2, old, new), [])
        del new["worker-page-reply/v2.schema.json"]
        self.assertTrue(any("all page and carry" in error for error in check("1.1.0", 1, 2, old, new)))


if __name__ == "__main__":
    unittest.main()
