#!/usr/bin/env python3
"""Unit tests for scripts/check_test262_ratchet.py."""

from __future__ import annotations

import importlib.util
import subprocess
import tempfile
import contextlib
import io
import sys
import unittest
import unittest.mock
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
        self.assertEqual(ratchet.regressions(self.base, head, {"a.js": "TS_NEW_CODE"}), [])
        self.assertEqual(len(ratchet.regressions(self.base, head, {"a.js": "TS_OTHER"})), 1)
        self.assertEqual(len(ratchet.regressions(self.base, head)), 1)
        failed = dict(self.base, **{"a.js": ("fail", "TS_NEW_CODE")})
        self.assertEqual(len(ratchet.regressions(self.base, failed, {"a.js": "TS_NEW_CODE"})), 1)

    def test_unreadable_base_and_unrelated_demotions_fail_closed(self):
        with tempfile.TemporaryDirectory(dir=ratchet.ROOT) as directory:
            root = Path(directory)
            def git(*args):
                return subprocess.run(["git", *args], cwd=root, check=True,
                                      capture_output=True, text=True).stdout.strip()
            git("init", "-q")
            outcomes = root / ratchet.OUTCOMES
            outcomes.parent.mkdir(parents=True)
            outcomes.write_text("test/a.js\tpass\t-\ntest/b.js\tpass\t-\n")
            git("add", ".")
            git("-c", "user.name=Test", "-c", "user.email=test@example.org",
                "commit", "-qm", "Base record")
            base = git("rev-parse", "HEAD")
            census = root / ratchet.CENSUS
            census.write_text("feature\tx\trejected\tTS_NEW_CODE\tprobe\n")
            skip = root / ratchet.TEST262 / "skip-register/feature/x.tsv"
            skip.parent.mkdir(parents=True)
            skip.write_text("test/a.js\tfeature:x\n")
            outcomes.write_text("test/a.js\trefused\tTS_NEW_CODE\ntest/b.js\trefused\tTS_NEW_CODE\n")
            results = []
            for ref in ["no-such-ref-xyz", base]:
                with unittest.mock.patch.object(ratchet, "ROOT", root), \
                     unittest.mock.patch.object(sys, "argv", ["ratchet", "--base", ref]), \
                     contextlib.redirect_stdout(io.StringIO()), \
                     contextlib.redirect_stderr(io.StringIO()):
                    try:
                        results.append(ratchet.main())
                    except SystemExit as error:
                        results.append(int(error.code) if isinstance(error.code, int) else 1)
            self.assertEqual(results, [1, 1], "bad refs and unnamed paths must be refused")
            outcomes.write_text("test/a.js\trefused\tTS_NEW_CODE\ntest/b.js\tpass\t-\n")
            with unittest.mock.patch.object(ratchet, "ROOT", root):
                self.assertEqual(ratchet.regressions(ratchet.base_outcomes(base),
                    ratchet.parse(outcomes.read_text()), ratchet.new_refusals(base)), [])

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

    def test_parse_shards_unions_the_record_files(self):
        outcomes = ratchet.parse_shards(
            [
                ("outcomes/built-ins/Object/propertyIsolation.tsv", "a.js\tpass\t-\nb.js\tfail\tFIG-1\n"),
                ("outcomes/language/expressions/assignment.tsv", "c.js\trefused\tTS_X\n"),
            ]
        )
        self.assertEqual(
            outcomes,
            {
                "a.js": ("pass", "-"),
                "b.js": ("fail", "FIG-1"),
                "c.js": ("refused", "TS_X"),
            },
        )

    def test_a_path_in_two_shards_exits(self):
        with self.assertRaises(SystemExit):
            ratchet.parse_shards(
                [
                    ("outcomes/built-ins/Object.tsv", "a.js\tpass\t-\n"),
                    ("outcomes/language/statements.tsv", "a.js\tfail\tFIG-1\n"),
                ]
            )

    def test_kernel_gate_requires_every_main_case_and_never_waives_a_refusal_code(self):
        refused = dict(self.base, **{"a.js": ("refused", "TS_NEW_CODE")})
        self.assertEqual(len(ratchet.kernel_regressions(self.base, refused, {})), 1)
        dropped = {path: outcome for path, outcome in self.base.items() if path != "d.js"}
        self.assertEqual(len(ratchet.kernel_regressions(self.base, dropped, {})), 1)
        with self.assertRaises(SystemExit):
            ratchet.parse("test/a.js\tpass\t-\ntest/a.js\tfail\tkernel\n")

    def test_kernel_gate_exempts_only_an_exact_registered_main_pass(self):
        failed = dict(self.base, **{"a.js": ("fail", "kernel")})
        self.assertEqual(ratchet.kernel_regressions(self.base, failed, {"a.js": "TS_RULE"}), [])
        self.assertEqual(len(ratchet.kernel_regressions(self.base, failed, {"b.js": "TS_RULE"})), 2)

    def test_complete_kernel_run_requires_exact_promotion_of_recovered_nonpasses(self):
        observed = {path: ("pass", "-") for path in self.base}
        expected = [
            "b.js: recorded `fail FIG-1`, kernel passed; promote this row to `pass -`",
            "c.js: recorded `refused TS_NEW_UNSUPPORTED`, kernel passed; promote this row to `pass -`",
            "d.js: recorded `harness program-size`, kernel passed; promote this row to `pass -`",
        ]
        self.assertEqual(ratchet.kernel_regressions(self.base, observed, {}), expected)
        # A filtered observation set cannot authorize promotion.
        self.assertEqual(
            ratchet.kernel_regressions(self.base, {"b.js": ("pass", "-")}, {}),
            [f"{path}: missing from the kernel run" for path in ("a.js", "c.js", "d.js")],
        )
        promoted = dict(self.base, **{"b.js": ("pass", "-")})
        self.assertEqual(ratchet.kernel_regressions(promoted, observed, {}), expected[1:])
        self.assertEqual(ratchet.kernel_regressions(observed, observed, {}), [])

        with tempfile.TemporaryDirectory(dir=ratchet.ROOT) as directory:
            outcomes = Path(directory) / "observations.tsv"
            outcomes.write_text("".join(f"{path}\tpass\t-\n" for path in observed))
            register = Path(directory) / "deviations.md"
            register.write_text("## Test262 exclusions\n")
            errors = io.StringIO()
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(errors):
                self.assertEqual(ratchet.check_kernel(self.base, outcomes, register), 1)
                self.assertEqual(ratchet.check_kernel(promoted, outcomes, register), 1)
                self.assertEqual(ratchet.check_kernel(observed, outcomes, register), 0)
            for problem in expected:
                self.assertIn(problem, errors.getvalue())

    def test_kernel_exclusions_require_a_real_unique_deviation(self):
        register = "| `TS_RULE` | kernel K-VAL-006 |\n## Test262 exclusions\n| `test/a.js` | `TS_RULE` |\n"
        self.assertEqual(ratchet.kernel_exclusions(register), {"test/a.js": "TS_RULE"})
        with self.assertRaises(ValueError):
            ratchet.kernel_exclusions(register.replace("test/a.js` | `TS_RULE", "test/a.js` | `TS_OTHER"))
        with self.assertRaises(ValueError):
            ratchet.kernel_exclusions(register + "| `test/a.js` | `TS_RULE` |\n")

    def test_outcome_paths_picks_up_both_record_layouts(self):
        listed = lambda ref, *paths: subprocess.CompletedProcess(
            args=[], returncode=0, stdout="\n".join(paths) + "\n", stderr=""
        )
        with unittest.mock.patch.object(ratchet.subprocess, "run") as run:
            run.return_value = listed("ref", "legacy/outcomes.tsv")
            run.return_value.stdout = (
                f"{ratchet.OUTCOMES_DIR}/built-ins.tsv\n{ratchet.OUTCOMES_DIR}/language.tsv\n"
            )
            self.assertEqual(
                ratchet.outcome_paths("deadbeef"),
                [f"{ratchet.OUTCOMES_DIR}/built-ins.tsv", f"{ratchet.OUTCOMES_DIR}/language.tsv"],
            )
            run.return_value.stdout = f"{ratchet.OUTCOMES}\n"
            self.assertEqual(ratchet.outcome_paths("deadbeef"), [ratchet.OUTCOMES])
            run.return_value.returncode = 1
            run.return_value.stdout = ""
            with self.assertRaises(SystemExit):
                ratchet.outcome_paths("deadbeef")


if __name__ == "__main__":
    unittest.main()
