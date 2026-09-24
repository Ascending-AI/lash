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

    def test_a_newly_registered_refusal_may_demote_a_pass(self):
        head = dict(self.base, **{"a.js": ("refused", "TS_NEW_CODE")})
        self.assertEqual(ratchet.regressions(self.base, head, frozenset({"TS_NEW_CODE"})), [])
        self.assertEqual(len(ratchet.regressions(self.base, head, frozenset({"TS_OTHER"}))), 1)
        self.assertEqual(len(ratchet.regressions(self.base, head)), 1)
        failed = dict(self.base, **{"a.js": ("fail", "TS_NEW_CODE")})
        self.assertEqual(len(ratchet.regressions(self.base, failed, frozenset({"TS_NEW_CODE"}))), 1)

    def test_tallies_derive_the_record_counts(self):
        self.assertEqual(
            ratchet.tallies(self.base),
            {
                ("selected", "-"): 4,
                ("pass", "*"): 1,
                ("fail", "*"): 1,
                ("fail", "FIG-1"): 1,
                ("refused", "*"): 1,
                ("refused", "TS_NEW_UNSUPPORTED"): 1,
                ("harness", "*"): 1,
                ("harness", "program-size"): 1,
            },
        )

    def test_rejected_rows_reads_the_census(self):
        census = "# kind\tname\tstatus\treason\tprobe\nfeature\tx\trejected\tTS_A\tp\nfeature\ty\taccepted\t-\t-\ntypescript\tz\trejected\tTS_B\tq\n"
        self.assertEqual(
            ratchet.rejected_rows(census),
            {("feature", "x"): "TS_A", ("typescript", "z"): "TS_B"},
        )

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
