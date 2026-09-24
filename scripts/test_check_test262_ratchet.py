#!/usr/bin/env python3
"""Unit tests for scripts/check_test262_ratchet.py."""

from __future__ import annotations

import importlib.util
import unittest
from pathlib import Path

sys_path = Path(__file__).with_name("check_test262_ratchet.py")
spec = importlib.util.spec_from_file_location("check_test262_ratchet", sys_path)
ratchet = importlib.util.module_from_spec(spec)
spec.loader.exec_module(ratchet)


class RatchetTest(unittest.TestCase):
    base = {
        "a.js": ("pass", "-"),
        "b.js": ("fail", "FIG-1"),
        "c.js": ("refused", "TS_NEW_UNSUPPORTED"),
        "d.js": ("harness", "program-size"),
    }

    def test_unchanged_and_improved_records_hold(self):
        self.assertEqual(ratchet.regressions(self.base, self.base), [])
        improved = dict(self.base, **{"b.js": ("pass", "-"), "c.js": ("pass", "-")})
        self.assertEqual(ratchet.regressions(self.base, improved), [])
        retitled = dict(self.base, **{"b.js": ("fail", "FIG-2")})
        self.assertEqual(ratchet.regressions(self.base, retitled), [])

    def test_a_lifted_refusal_may_expose_a_failure(self):
        lifted = dict(self.base, **{"c.js": ("fail", "FIG-3"), "d.js": ("fail", "FIG-4")})
        self.assertEqual(ratchet.regressions(self.base, lifted), [])

    def test_a_lost_pass_is_a_regression(self):
        for outcome in [("fail", "FIG-1"), ("refused", "TS_NEW_UNSUPPORTED"), ("harness", "x")]:
            self.assertEqual(
                len(ratchet.regressions(self.base, dict(self.base, **{"a.js": outcome}))), 1, outcome
            )
        dropped = {path: outcome for path, outcome in self.base.items() if path != "a.js"}
        self.assertEqual(len(ratchet.regressions(self.base, dropped)), 1)

    def test_parse_reads_the_record_format(self):
        text = "# test262-path\tclass\tqualifier\na.js\tpass\t-\nb.js\tfail\tFIG-1\n"
        self.assertEqual(ratchet.parse(text), {"a.js": ("pass", "-"), "b.js": ("fail", "FIG-1")})


if __name__ == "__main__":
    unittest.main()
