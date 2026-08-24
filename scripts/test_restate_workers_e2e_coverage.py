#!/usr/bin/env python3
from __future__ import annotations

import tempfile
from pathlib import Path
import unittest

from check_restate_workers_e2e_coverage import Leg, verify


class RestateWorkersE2ECoverageTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.inventory = "1\te2e-main\n1\te2e-main-wake\n2\te2e-tool-batch\n"

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def write(self, name: str, contents: str) -> Path:
        path = self.root / name
        path.write_text(contents, encoding="utf-8")
        return path

    def legs(self, segment_one: str, segment_two: str) -> list[Leg]:
        return [
            Leg(1, self.write("inventory-1.tsv", self.inventory), self.write("one.txt", segment_one)),
            Leg(2, self.write("inventory-2.tsv", self.inventory), self.write("two.txt", segment_two)),
        ]

    def test_exact_union_passes(self) -> None:
        verify(self.legs("e2e-main\ne2e-main-wake\n", "e2e-tool-batch\n"))

    def test_missing_workflow_fails(self) -> None:
        with self.assertRaisesRegex(ValueError, "missing=\\['e2e-main-wake'\\]"):
            verify(self.legs("e2e-main\n", "e2e-tool-batch\n"))

    def test_wrong_segment_fails(self) -> None:
        with self.assertRaisesRegex(ValueError, "unexpected=\\['e2e-tool-batch'\\]"):
            verify(self.legs("e2e-main\ne2e-main-wake\ne2e-tool-batch\n", "e2e-tool-batch\n"))

    def test_one_manifest_cannot_satisfy_summary(self) -> None:
        inventory = self.write("inventory-1.tsv", self.inventory)
        completed = self.write("one.txt", "e2e-main\ne2e-main-wake\n")
        with self.assertRaisesRegex(ValueError, "segments 1 and 2"):
            verify([Leg(1, inventory, completed)])

    def test_inventory_drift_between_legs_fails(self) -> None:
        legs = self.legs("e2e-main\ne2e-main-wake\n", "e2e-tool-batch\n")
        legs[1].inventory_path.write_text(
            self.inventory + "2\te2e-extra\n", encoding="utf-8"
        )
        with self.assertRaisesRegex(ValueError, "inventories differ"):
            verify(legs)


if __name__ == "__main__":
    unittest.main()
