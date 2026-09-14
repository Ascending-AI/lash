from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("check-postgres-json-carrier-coverage.py")
SPEC = importlib.util.spec_from_file_location("postgres_json_carrier_coverage", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class PostgresJsonCarrierCoverageTest(unittest.TestCase):
    def test_repository_inventory_is_complete(self) -> None:
        carriers = MODULE.schema_carriers(MODULE.SCHEMA.read_text(encoding="utf-8"))
        enrolled = MODULE.enrolled_carriers(MODULE.SHAPE.read_text(encoding="utf-8"))
        manifest = MODULE.json.loads(MODULE.MANIFEST.read_text(encoding="utf-8"))
        valid, errors = MODULE.validate(carriers, enrolled, manifest)
        self.assertTrue(valid, errors)
        self.assertEqual(len(carriers), 27)

    def test_unclassified_schema_carrier_fails(self) -> None:
        valid, errors = MODULE.validate(
            {"lash_example.payload_json"}, set(), {}
        )
        self.assertFalse(valid)
        self.assertIn(
            "unclassified PostgreSQL JSON carrier: lash_example.payload_json", errors
        )

    def test_enrollment_must_match_published_payload_shape(self) -> None:
        manifest = {
            "lash_example.payload_json": {
                "verdict": "enroll-now",
                "reason": "example",
            }
        }
        valid, errors = MODULE.validate(
            {"lash_example.payload_json"}, set(), manifest
        )
        self.assertFalse(valid)
        self.assertIn(
            "enroll-now carrier has no payload shape: lash_example.payload_json", errors
        )


if __name__ == "__main__":
    unittest.main()
