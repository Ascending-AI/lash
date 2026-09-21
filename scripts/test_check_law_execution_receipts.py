#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check_law_execution_receipts.py")
SPEC = importlib.util.spec_from_file_location("check_law_execution_receipts", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


MACROS = """\
#[macro_export]
macro_rules! plain_suite_tests {
    ($fixture:block) => {
        $crate::plain_suite_tests!(@catalogue $fixture);
        $crate::plain_suite_tests!(@extra $fixture; [(extra_law, "extra")]);
    };
    (@catalogue $fixture:block) => {
        $(
            #[tokio::test]
            async fn $law() {
                let (_g, make) = $fixture;
                $crate::support::$law(make).await;
                $crate::law_receipt::record(stringify!($law), $label);
            }
        )*
    };
    (@catalogue $fixture:block) => {
        // Second arm sharing the head name: both arms' rows count.
        $crate::__helper! { $fixture; [(shared_law, "shared"), (timed_law, "timed")] }
    };
    (@extra $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
    };
}

#[macro_export]
macro_rules! reopenable_suite_tests {
    ($fixture:block) => {
        $crate::plain_suite_tests!(@catalogue $fixture);
        $crate::reopenable_suite_tests!(@reopen $fixture; [(reopen_law, "reopen")]);
    };
    (@reopen $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
    };
}
"""


class MacroParsingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.macros = MODULE.macro_blocks(MACROS)

    def test_arms_split_on_top_level_pattern_groups(self) -> None:
        plain = self.macros["plain_suite_tests"]
        # The two @catalogue arms share a head; the dict keeps the last one's
        # body but expected() scans every arm body, so all rows count.
        self.assertIn(("shared_law", "shared"), MODULE.suite_expected(self.macros, "plain_suite_tests"))
        self.assertIn(("extra_law", "extra"), MODULE.suite_expected(self.macros, "plain_suite_tests"))

    def test_delegated_arm_rows_are_inherited(self) -> None:
        expected = MODULE.suite_expected(self.macros, "reopenable_suite_tests")
        self.assertIn(("reopen_law", "reopen"), expected)
        self.assertIn(("shared_law", "shared"), expected)
        # @extra is not delegated to by the reopenable entry arm.
        self.assertNotIn(("extra_law", "extra"), expected)

    def test_helper_macros_are_not_suites(self) -> None:
        registered = MODULE.registered_pairs(self.macros)
        suites = {s for suites in registered.values() for s in suites}
        self.assertNotIn("__helper", suites)


class InvocationTests(unittest.TestCase):
    def test_comment_negations_do_not_claim_a_suite(self) -> None:
        text = """\
// No store_effect_group_drain_tests!: the drain is one implementation.
lash_conformance::effect_group_host_tests!({
    (guard, factory)
});
"""
        self.assertEqual(MODULE.invoked_suites(text), {"effect_group_host_tests"})

    def test_source_files_follow_path_and_mod_includes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "sub").mkdir()
            main = root / "main.rs"
            main.write_text(
                '#[path = "included.rs"]\nmod included;\nmod sub;\n',
                encoding="utf-8",
            )
            (root / "included.rs").write_text("", encoding="utf-8")
            (root / "sub" / "mod.rs").write_text("", encoding="utf-8")
            files = MODULE.source_files(main)
            self.assertEqual(
                {f.name for f in files}, {"main.rs", "included.rs", "mod.rs"}
            )


class ReceiptTests(unittest.TestCase):
    def test_receipt_lines_parse_law_and_label(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "receipts.txt"
            path.write_text(
                "some_law\tlabel-a\n\nother_law\tlabel-b\n", encoding="utf-8"
            )
            self.assertEqual(
                MODULE.read_receipts([path]),
                {("some_law", "label-a"), ("other_law", "label-b")},
            )


class RealTreeTests(unittest.TestCase):
    """The real macros.rs keeps its delegation invariants under the census."""

    def test_reopenable_inherits_shared_catalogue_only(self) -> None:
        macros = MODULE.macro_blocks(MODULE.MACROS.read_text(encoding="utf-8"))
        plain = MODULE.suite_expected(macros, "runtime_persistence_tests")
        reopenable = MODULE.suite_expected(
            macros, "runtime_persistence_reopenable_tests"
        )
        self.assertIn(("reopen_mint_identity", "pending-turn-input-multi-store-mint"), reopenable)
        self.assertNotIn(("reopen_mint_identity", "pending-turn-input-multi-store-mint"), plain)
        self.assertIn(("fresh_instances", "fresh-instance-probe"), plain)
        self.assertNotIn(("fresh_instances", "fresh-instance-probe"), reopenable)
        self.assertTrue(plain - reopenable, "plain must keep its own rows")
        self.assertTrue(reopenable - plain, "reopenable must keep its own rows")


if __name__ == "__main__":
    unittest.main()
