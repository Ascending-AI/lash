#!/usr/bin/env python3
"""Tests for exact feature-variant resolution from the Buck2 inventory."""

from __future__ import annotations

import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest import mock


SCRIPT = Path(__file__).with_name("resolve_buck2_target.py")
SPEC = importlib.util.spec_from_file_location("resolve_buck2_target", SCRIPT)
assert SPEC and SPEC.loader
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


class ResolveBuck2TargetTests(unittest.TestCase):
    def resolve(self, units: list[dict], *argv: str) -> tuple[int, str]:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            inventory = root / "tools/buck2/target-inventory.json"
            inventory.parent.mkdir(parents=True)
            inventory.write_text(json.dumps({"feature_lane_units": units}))
            with mock.patch.object(MODULE, "ROOT", root), mock.patch(
                "sys.argv", [str(SCRIPT), *argv]
            ), mock.patch("builtins.print") as output:
                try:
                    status = MODULE.main()
                except SystemExit as error:
                    return int(error.code), ""
            return status, str(output.call_args.args[0])

    def test_selects_the_exact_feature_set(self) -> None:
        units = [
            {"label": "//pkg:tool__fv_base", "features": []},
            {"label": "//pkg:tool__fv_next", "features": ["synthetic-next"]},
        ]
        self.assertEqual(
            (0, "//pkg:tool__fv_next"),
            self.resolve(units, "//pkg", "tool", "--feature", "synthetic-next"),
        )

    def test_refuses_missing_and_ambiguous_matches(self) -> None:
        unit = {"label": "//pkg:tool__fv_a", "features": ["next"]}
        self.assertEqual(2, self.resolve([unit], "//pkg", "tool")[0])
        self.assertEqual(2, self.resolve([unit, unit | {"label": "//pkg:tool__fv_b"}], "//pkg", "tool", "--feature", "next")[0])


if __name__ == "__main__":
    unittest.main()
