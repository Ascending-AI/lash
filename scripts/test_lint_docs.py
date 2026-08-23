#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("lint_docs.py")
SPEC = importlib.util.spec_from_file_location("lint_docs", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


class AdrFilenameCheckTest(unittest.TestCase):
    def check(self, *relative_paths: str) -> list[str]:
        with tempfile.TemporaryDirectory() as directory:
            adr_dir = Path(directory) / "docs" / "adr"
            for relative_path in relative_paths:
                path = adr_dir / relative_path
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("# Planted ADR\n", encoding="utf-8")
            errors: list[str] = []
            MODULE.check_adr_number_uniqueness(errors, adr_dir=adr_dir)
            return errors

    def test_canonical_unique_names_pass(self) -> None:
        self.assertEqual(
            self.check("0034-one.md", "0035-two.md"),
            [],
        )

    def test_current_non_adr_allowlist_is_empty(self) -> None:
        self.assertEqual(MODULE.ADR_NON_ADR_ALLOWLIST, frozenset())

    def test_duplicate_numbers_still_fail(self) -> None:
        self.assertEqual(
            self.check("0034-one.md", "0034-two.md"),
            [
                "docs/adr: duplicate ADR number 0034: "
                "docs/adr/0034-one.md, docs/adr/0034-two.md"
            ],
        )

    def test_malformed_filename_shapes_fail(self) -> None:
        planted = (
            "0034.md",
            "0034-slug.MD",
            "sub/0034-x.md",
            "00341-x.md",
            "٠٠٣٤-x.md",
        )
        errors = self.check(*planted)
        self.assertEqual(
            errors,
            [
                f"docs/adr/{relative}: filename must match NNNN-slug.md"
                for relative in sorted(planted)
            ],
        )
        for error in errors:
            print(f"planted docs lint: {error}")


if __name__ == "__main__":
    unittest.main()
