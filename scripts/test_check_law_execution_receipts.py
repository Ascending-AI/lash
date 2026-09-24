#!/usr/bin/env python3

from __future__ import annotations

from collections import Counter
import importlib.util
from pathlib import Path
import re
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check_law_execution_receipts.py")
SPEC = importlib.util.spec_from_file_location("check_law_execution_receipts", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)

ROOT = Path(__file__).resolve().parents[1]
JUSTFILE = ROOT / "justfile"
CI_YML = ROOT / ".github" / "workflows" / "ci.yml"


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
                $crate::law_receipt::record(module_path!(), stringify!($law), $label);
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

PLAIN_SUITE_ROWS = {
    ("shared_law", "shared"),
    ("timed_law", "timed"),
    ("extra_law", "extra"),
}


def receipts_text(observed: dict[str, Counter]) -> str:
    lines = []
    for claimant, counter in observed.items():
        for (law, label), count in counter.items():
            lines.extend([f"{claimant}\t{law}\t{label}"] * count)
    return "\n".join(lines) + "\n"


def full_receipts(claimant: str) -> dict[str, Counter]:
    return {claimant: Counter(PLAIN_SUITE_ROWS)}


class MacroParsingTests(unittest.TestCase):
    def setUp(self) -> None:
        self.macros = MODULE.macro_blocks(MACROS)

    def test_arms_split_on_top_level_pattern_groups(self) -> None:
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


class ModuleWalkTests(unittest.TestCase):
    def test_comment_negations_do_not_claim_a_suite(self) -> None:
        text = """\
// No store_effect_group_drain_tests!: the drain is one implementation.
lash_conformance::effect_group_host_tests!({
    (guard, factory)
});
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "laws.rs"
            root.write_text(text, encoding="utf-8")
            invocations = MODULE.invocations_in_root(root, "laws")
        self.assertEqual(
            [(inv.suite, inv.claimant) for inv in invocations],
            [("effect_group_host_tests", "laws")],
        )

    def test_block_comments_do_not_claim_a_suite(self) -> None:
        text = """\
/* retired: suite_a_tests!({ fixture }); */
suite_b_tests!({ fixture });
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "laws.rs"
            root.write_text(text, encoding="utf-8")
            invocations = MODULE.invocations_in_root(root, "laws")
        self.assertEqual([inv.suite for inv in invocations], ["suite_b_tests"])

    def test_inline_modules_extend_the_claimant(self) -> None:
        text = """\
mod native {
    suite_a_tests!({ fixture });
}
mod sqlite {
    suite_a_tests!({ fixture });
}
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "laws.rs"
            root.write_text(text, encoding="utf-8")
            invocations = MODULE.invocations_in_root(root, "laws")
        self.assertEqual(
            sorted(inv.claimant for inv in invocations),
            ["laws::native", "laws::sqlite"],
        )

    def test_mod_decl_from_a_plain_file_resolves_under_its_stem(self) -> None:
        """`mod x;` in `tests.rs` is `tests/x.rs`, never `x.rs` beside it."""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "tests").mkdir()
            main = root / "lib.rs"
            main.write_text("mod tests;\n", encoding="utf-8")
            (root / "tests.rs").write_text("mod child;\n", encoding="utf-8")
            (root / "tests" / "child.rs").write_text(
                "suite_a_tests!({ fixture });\n", encoding="utf-8"
            )
            invocations = MODULE.invocations_in_root(main, "pkg")
        self.assertEqual(
            [(inv.suite, inv.claimant) for inv in invocations],
            [("suite_a_tests", "pkg::tests::child")],
        )

    def test_mod_decl_from_mod_rs_resolves_beside_it(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / "sub").mkdir()
            main = root / "main.rs"
            main.write_text("mod sub;\n", encoding="utf-8")
            (root / "sub" / "mod.rs").write_text("mod child;\n", encoding="utf-8")
            (root / "sub" / "child.rs").write_text(
                "suite_a_tests!({ fixture });\n", encoding="utf-8"
            )
            invocations = MODULE.invocations_in_root(main, "pkg")
        self.assertEqual(
            [(inv.suite, inv.claimant) for inv in invocations],
            [("suite_a_tests", "pkg::sub::child")],
        )

    def test_path_attribute_redirects_a_mod_decl(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            main = root / "main.rs"
            main.write_text(
                '#[path = "included.rs"]\nmod included;\n', encoding="utf-8"
            )
            (root / "included.rs").write_text(
                "suite_a_tests!({ fixture });\n", encoding="utf-8"
            )
            invocations = MODULE.invocations_in_root(main, "pkg")
        self.assertEqual(
            [(inv.suite, inv.claimant) for inv in invocations],
            [("suite_a_tests", "pkg::included")],
        )

    def test_an_ignored_invocation_is_flagged_not_dropped(self) -> None:
        text = """\
suite_a_tests!(
    #[ignore = "requires an isolated server; run by `just e2e`"]
    { fixture_a }
);
suite_b_tests!({ fixture_b });
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "laws.rs"
            root.write_text(text, encoding="utf-8")
            invocations = MODULE.invocations_in_root(root, "laws")
        by_suite = {inv.suite: inv.ignored for inv in invocations}
        self.assertEqual(by_suite, {"suite_a_tests": True, "suite_b_tests": False})

    def test_one_live_invocation_keeps_the_suite_claimed(self) -> None:
        text = """\
suite_a_tests!(#[ignore = "deferred"] { fixture_a });
suite_a_tests!({ fixture_b });
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "laws.rs"
            root.write_text(text, encoding="utf-8")
            invocations = MODULE.invocations_in_root(root, "laws")
        self.assertEqual(len(invocations), 2)
        live = [inv for inv in invocations if not inv.ignored]
        self.assertEqual(len(live), 1)


class CensusTests(unittest.TestCase):
    def setUp(self) -> None:
        self.macros = MODULE.macro_blocks(MACROS)

    def test_a_missing_claimant_fails_naming_the_claimant(self) -> None:
        """Two invocations of one suite; receipts for only `native` must fail
        naming `laws::sqlite` -- one receipt never satisfies a sibling."""
        text = """\
mod native {
    plain_suite_tests!({ fixture });
}
mod sqlite {
    plain_suite_tests!({ fixture });
}
"""
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "laws.rs"
            root.write_text(text, encoding="utf-8")
            invocations = MODULE.invocations_in_root(root, "laws")
            expected = MODULE.expected_from_invocations(invocations, self.macros)
            observed = full_receipts("laws::native")
            errors = MODULE.census_compare(expected, observed, "")
        self.assertTrue(
            any("laws::sqlite" in error for error in errors),
            f"the unreceipted sibling claimant must be named: {errors}",
        )

    def test_a_two_column_receipt_line_is_an_error(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "receipts.txt"
            path.write_text("some_law\tlabel-a\n", encoding="utf-8")
            _, errors = MODULE.read_receipts([path])
        self.assertTrue(
            any("pre-FIG-3472" in error for error in errors),
            f"a 2-column line must be rejected as the old format: {errors}",
        )

    def test_receipt_lines_parse_claimant_law_label(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "receipts.txt"
            path.write_text(
                "crate_a::tier\tlaw\tlabel-a\ncrate_b\tlaw\tlabel-a\n",
                encoding="utf-8",
            )
            observed, errors = MODULE.read_receipts([path])
        self.assertEqual(errors, [])
        self.assertEqual(
            observed,
            {
                "crate_a::tier": Counter({("law", "label-a"): 1}),
                "crate_b": Counter({("law", "label-a"): 1}),
            },
        )

    def test_a_duplicate_receipt_fails(self) -> None:
        expected = full_receipts("laws")
        observed = full_receipts("laws")
        observed["laws"][("extra_law", "extra")] += 1
        errors = MODULE.census_compare(expected, observed, "")
        self.assertTrue(any("duplicate" in error for error in errors), errors)

    def test_a_receipt_for_an_unowed_law_fails(self) -> None:
        expected = full_receipts("laws")
        observed = full_receipts("laws")
        observed["laws"][("other_law", "other")] += 1
        errors = MODULE.census_compare(expected, observed, "")
        self.assertTrue(any("other_law" in error for error in errors), errors)

    def test_a_receipt_under_the_wrong_claimant_fails(self) -> None:
        expected = full_receipts("laws::native")
        observed = full_receipts("laws::sqlite")
        errors = MODULE.census_compare(expected, observed, "")
        self.assertTrue(errors, "a sibling claimant's receipts must not satisfy")

    def test_a_repeated_crate_claim_does_not_double_count(self) -> None:
        """`--crate X --crate X` is one expectation, not two."""
        with tempfile.TemporaryDirectory() as tmp:
            crate = Path(tmp) / "fakepkg"
            (crate / "tests").mkdir(parents=True)
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "fakepkg"\n', encoding="utf-8"
            )
            (crate / "tests" / "laws.rs").write_text(
                "plain_suite_tests!({ fixture });\n", encoding="utf-8"
            )
            once = MODULE.expected_from_invocations(
                MODULE.invocations_in_crate(crate), self.macros
            )
            twice = MODULE.expected_from_invocations(
                MODULE.invocations_in_crate(crate)
                + MODULE.invocations_in_crate(crate),
                self.macros,
            )
        self.assertEqual(once, twice)
        self.assertEqual(twice, {"laws": Counter(PLAIN_SUITE_ROWS)})


class BazelLabelResolutionTests(unittest.TestCase):
    def test_unit_test_label_excludes_tests_main_integration_root(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            crate = Path(tmp) / "fakepkg"
            (crate / "src").mkdir(parents=True)
            (crate / "tests").mkdir()
            (crate / "Cargo.toml").write_text(
                '[package]\nname = "fakepkg"\n', encoding="utf-8"
            )
            lib = crate / "src" / "lib.rs"
            lib.write_text("", encoding="utf-8")
            (crate / "tests" / "main.rs").write_text("", encoding="utf-8")

            roots = MODULE.resolve_bazel_label(crate, "fakepkg__unit_test")

        self.assertEqual(roots, [("fakepkg", lib)])


class BazelBatchCensusTests(unittest.TestCase):
    """A `test_batch` target is censused over the union of its members."""

    def setUp(self) -> None:
        self.macros = MODULE.macro_blocks(MACROS)
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self._old_root = MODULE.ROOT
        MODULE.ROOT = self.root

    def tearDown(self) -> None:
        MODULE.ROOT = self._old_root
        self._tmp.cleanup()

    def fixture(self) -> Path:
        crate = self.root / "crates" / "fakepkg"
        (crate / "tests").mkdir(parents=True)
        (crate / "Cargo.toml").write_text(
            '[package]\nname = "fakepkg"\n', encoding="utf-8"
        )
        (crate / "tests" / "laws.rs").write_text(
            "lash_conformance::plain_suite_tests!({ fixture });\n",
            encoding="utf-8",
        )
        target_dir = self.root / "testlogs" / "crates" / "fakepkg" / "test_batch"
        (target_dir / "test.outputs").mkdir(parents=True)
        (target_dir / "test.log").write_text("ok", encoding="utf-8")
        return target_dir

    def batch(self) -> dict[str, list[str]]:
        return {"//crates/fakepkg:test_batch": ["//crates/fakepkg:laws__test"]}

    def census(self) -> list[str]:
        errors, _ = MODULE.census_testlogs(
            self.root / "testlogs", self.batch(), self.macros, set()
        )
        return errors

    def test_batch_member_missing_one_receipt_fails(self) -> None:
        target_dir = self.fixture()
        observed = full_receipts("laws")
        del observed["laws"][("timed_law", "timed")]
        (target_dir / "test.outputs" / "law-receipts.txt").write_text(
            receipts_text(observed), encoding="utf-8"
        )
        errors = self.census()
        self.assertTrue(
            any("`timed_law`" in error for error in errors),
            f"a member law without a receipt must fail the census: {errors}",
        )

    def test_batch_member_fully_receipted_passes(self) -> None:
        target_dir = self.fixture()
        (target_dir / "test.outputs" / "law-receipts.txt").write_text(
            receipts_text(full_receipts("laws")), encoding="utf-8"
        )
        self.assertEqual(self.census(), [])

    def test_unresolvable_ran_target_with_receipts_fails(self) -> None:
        target_dir = self.fixture()
        (target_dir / "test.outputs" / "law-receipts.txt").write_text(
            receipts_text(full_receipts("laws")), encoding="utf-8"
        )
        errors, _ = MODULE.census_testlogs(
            self.root / "testlogs",
            {"//crates/fakepkg:test_batch": ["//crates/fakepkg:gone__test"]},
            self.macros,
            set(),
        )
        self.assertTrue(
            any("resolved to a test root" in error for error in errors),
            f"a ran target with receipts it cannot account for must fail: {errors}",
        )


class DeferredCensusTests(unittest.TestCase):
    """``--deferred`` must owe every manifest suite, not just the first.

    Two manifest entries sharing one file and claimant must stay two
    distinct expectations: the deferred claim resolves each entry to its
    real ignored invocation (true file and line), so receipt-covering both
    passes and covering only one fails naming the other's laws.
    """

    MACROS = """\
macro_rules! suite_a_tests {
    ($fixture:block) => {};
    (@catalogue $fixture:block) => { [(law_a1, "a1"), (law_a2, "a2")] };
}
macro_rules! suite_b_tests {
    ($fixture:block) => {};
    (@catalogue $fixture:block) => { [(law_b1, "b1")] };
}
"""

    def setUp(self) -> None:
        self.macros = MODULE.macro_blocks(self.MACROS)
        self._tmp = tempfile.TemporaryDirectory()
        self.root = Path(self._tmp.name)
        self._old_root = MODULE.ROOT
        self._old_manifest = MODULE.DEFERRED_MANIFEST
        MODULE.ROOT = self.root
        crate = self.root / "crates" / "fakepkg"
        (crate / "src").mkdir(parents=True)
        (crate / "Cargo.toml").write_text(
            '[package]\nname = "fakepkg"\n', encoding="utf-8"
        )
        (crate / "src" / "lib.rs").write_text(
            'suite_a_tests!(#[ignore = "deferred"] { f });\n'
            'suite_b_tests!(#[ignore = "deferred"] { f });\n',
            encoding="utf-8",
        )
        MODULE.DEFERRED_MANIFEST = self.root / "deferred.toml"
        MODULE.DEFERRED_MANIFEST.write_text(
            """\
[[deferred]]
file = "crates/fakepkg/src/lib.rs"
claimant = "fakepkg"
suite = "suite_a_tests"
recipe = "deferred-e2e"

[[deferred]]
file = "crates/fakepkg/src/lib.rs"
claimant = "fakepkg"
suite = "suite_b_tests"
recipe = "deferred-e2e"
""",
            encoding="utf-8",
        )

    def tearDown(self) -> None:
        MODULE.ROOT = self._old_root
        MODULE.DEFERRED_MANIFEST = self._old_manifest
        self._tmp.cleanup()

    def deferred_expected(self) -> dict[str, Counter]:
        errors: list[str] = []
        index = MODULE.manifest_check(errors)
        self.assertEqual(errors, [])
        invocations, error = MODULE.deferred_invocations("deferred-e2e", index)
        self.assertIsNone(error)
        return MODULE.expected_from_invocations(invocations, self.macros)

    def test_receipts_for_both_suites_pass(self) -> None:
        expected = self.deferred_expected()
        observed = {
            "fakepkg": Counter(
                {("law_a1", "a1"): 1, ("law_a2", "a2"): 1, ("law_b1", "b1"): 1}
            )
        }
        self.assertEqual(MODULE.census_compare(expected, observed, ""), [])

    def test_receipts_for_one_suite_fail_naming_the_other(self) -> None:
        expected = self.deferred_expected()
        observed = {"fakepkg": Counter({("law_a1", "a1"): 1, ("law_a2", "a2"): 1})}
        errors = MODULE.census_compare(expected, observed, "")
        self.assertTrue(
            any("law_b1" in error and "fakepkg" in error for error in errors),
            f"the second suite's laws must be owed under the claimant: {errors}",
        )


class ParkedEntryTests(unittest.TestCase):
    KEY = ("crates/x/src/tests.rs", "x::tests", "demo_tests")

    def test_a_parked_entry_names_its_ticket_and_reason(self) -> None:
        entry = {"recipe": "parked", "ticket": "FIG-1", "reason": "why"}
        self.assertEqual(MODULE.parked_entry_errors(self.KEY, entry), [])

    def test_a_parked_entry_without_ticket_or_reason_is_refused(self) -> None:
        errors = MODULE.parked_entry_errors(self.KEY, {"recipe": "parked"})
        self.assertEqual(len(errors), 2)
        errors = MODULE.parked_entry_errors(
            self.KEY, {"recipe": "parked", "ticket": "someday", "reason": "why"}
        )
        self.assertEqual(len(errors), 1)

    def test_the_real_parked_restate_laws_are_skipped_by_name(self) -> None:
        skips = MODULE.parked_skips("lash_restate", MODULE.load_macros())
        self.assertEqual(
            skips,
            [
                "--skip",
                "tests::conformance_and_poison::direct_turn_acceptance_crash_after_store_commit_admits_one_row",
                "--skip",
                "tests::conformance_and_poison::turn_crash_after_commit_redrive_replays_the_committed_receipt",
                "--skip",
                "tests::conformance_and_poison::turn_crash_matrix_error_return_fail_stop",
                "--skip",
                "tests::conformance_and_poison::turn_crash_matrix_level_1",
            ],
        )
        self.assertEqual(MODULE.parked_skips("no_such_crate", MODULE.load_macros()), [])

    def test_a_live_entry_may_not_carry_a_ticket(self) -> None:
        entry = {"recipe": "effect-group-conformance-e2e", "ticket": "FIG-1"}
        self.assertEqual(len(MODULE.parked_entry_errors(self.KEY, entry)), 1)


class RealTreeTests(unittest.TestCase):
    """The real macros.rs keeps its delegation invariants under the census."""

    def test_response_derivation_catalogue_has_only_backend_claimants(self) -> None:
        macros = MODULE.load_macros()
        self.assertEqual(
            MODULE.suite_expected(macros, "effect_host_tests"),
            {("effect_host", "effect-host")},
        )
        laws = MODULE.suite_expected(macros, "effect_controller_response_derivation_tests")
        self.assertEqual(
            laws,
            {
                ("effect_controller_response_derivation_retry", "effect-controller-response-derivation-retry"),
                ("effect_controller_response_derivation_terminals", "effect-controller-response-derivation-terminals"),
            },
        )
        # The SQLite suite is one module mounted by two binaries, one per
        # backend substrate (ADR 0102), so each claims it under its own path.
        for label, claimant in (
            ("//crates/lash-sqlite-store:conformance__test", "conformance::suite"),
            ("//crates/lash-sqlite-store:conformance_memory__test", "conformance_memory::suite"),
            ("//crates/lash-postgres-store:conformance__test", "conformance"),
        ):
            invocations = [
                invocation
                for prefix, root in MODULE.resolve_label(label)
                for invocation in MODULE.invocations_in_root(root, prefix)
            ]
            expected = MODULE.expected_from_invocations(invocations, macros)
            self.assertEqual({pair for pair in expected[claimant] if pair in laws}, laws)
        invocations = [
            invocation
            for prefix, root in MODULE.resolve_label(
                "//crates/lash-conformance:lash-conformance__unit_test"
            )
            for invocation in MODULE.invocations_in_root(root, prefix)
        ]
        expected = MODULE.expected_from_invocations(invocations, macros)
        self.assertFalse(any(pair in laws for pairs in expected.values() for pair in pairs))

    def test_reopenable_inherits_shared_catalogue_only(self) -> None:
        macros = MODULE.load_macros()
        plain = MODULE.suite_expected(macros, "runtime_persistence_tests")
        reopenable = MODULE.suite_expected(
            macros, "runtime_persistence_reopenable_tests"
        )
        self.assertIn(("reopen_mint_identity", "pending-turn-input-multi-store-mint"), reopenable)
        self.assertNotIn(("reopen_mint_identity", "pending-turn-input-multi-store-mint"), plain)
        self.assertTrue(plain, "the shared catalogue keeps its rows")
        self.assertLessEqual(
            plain, reopenable, "the reopenable suite registers the whole shared catalogue"
        )
        self.assertTrue(reopenable - plain, "reopenable must keep its own rows")

    def test_process_registry_reopen_law_is_a_catalogue_row(self) -> None:
        macros = MODULE.load_macros()
        reopenable = MODULE.suite_expected(
            macros, "process_registry_reopenable_tests"
        )
        self.assertIn(
            ("process_registry_reopen_conformance", "process-registry-reopen"),
            reopenable,
        )

    def test_a_catalogue_declared_beside_its_laws_is_owed(self) -> None:
        """``drain_end_tests!`` lives in ``conformance/drain_end.rs``, not
        ``macros.rs``; its rows must still be registered pairs, or every
        receipt the suite writes is rejected as misclaimed (FIG-3419)."""
        self.assertNotIn(
            "macro_rules! drain_end_tests",
            MODULE.MACROS.read_text(encoding="utf-8"),
            "the precondition: this catalogue is outside macros.rs",
        )
        macros = MODULE.load_macros()
        expected = MODULE.suite_expected(macros, "drain_end_tests")
        self.assertEqual(len(expected), 10)
        self.assertIn(
            ("a_crash_after_the_drain_receipt_recovers_its_ledger_row", "drain-end-crash-window"),
            expected,
        )
        self.assertIn(
            (
                "a_durably_failed_drain_settles_its_closing_group_and_ends",
                "drain-end-durably-failed",
            ),
            expected,
        )
        self.assertIn(
            (
                "an_abandoned_drain_settles_its_closing_group_and_ends",
                "drain-end-abandoned",
            ),
            expected,
        )
        self.assertIn(
            (
                "a_failed_drain_ends_once_its_foreign_closing_work_settles",
                "drain-end-owed-after-failed",
            ),
            expected,
        )

    def test_the_deferred_manifest_names_real_ignored_invocations(self) -> None:
        errors: list[str] = []
        manifest_set = MODULE.manifest_check(errors)
        self.assertEqual(errors, [])
        self.assertEqual(len(manifest_set), 7)
        self.assertIn(
            (
                "crates/lash-restate/src/tests/conformance_and_poison.rs",
                "lash_restate::tests::conformance_and_poison",
                "effect_group_host_tests",
            ),
            manifest_set,
        )

    def test_the_real_deferred_recipe_owes_every_manifest_suite(self) -> None:
        """A receipts file covering all six manifest suites passes, and
        dropping one suite's rows fails naming that suite's laws -- the
        entries share one file and claimant, so this pins the deferred
        claim resolving each entry to its own real invocation."""
        macros = MODULE.load_macros()
        errors: list[str] = []
        index = MODULE.manifest_check(errors)
        self.assertEqual(errors, [])
        invocations, error = MODULE.deferred_invocations(
            "effect-group-conformance-e2e", index
        )
        self.assertIsNone(error)
        self.assertEqual(len(invocations), 6)
        expected = MODULE.expected_from_invocations(invocations, macros)
        observed: dict[str, Counter] = {
            claimant: Counter(pairs) for claimant, pairs in expected.items()
        }
        self.assertEqual(MODULE.census_compare(expected, observed, ""), [])
        dropped_suite = "effect_host_await_event_witness_tests"
        dropped = MODULE.suite_expected(macros, dropped_suite)
        self.assertTrue(dropped)
        claimant = "lash_restate::tests::conformance_and_poison"
        for pair in dropped:
            del observed[claimant][pair]
        census_errors = MODULE.census_compare(expected, observed, "")
        self.assertTrue(
            any(
                pair[0] in error and claimant in error
                for pair in dropped
                for error in census_errors
            ),
            f"dropping {dropped_suite} rows must fail naming its laws: "
            f"{census_errors}",
        )


def recipe_block(justfile: str, recipe: str) -> str:
    match = re.search(rf"^{re.escape(recipe)}:\n(?P<body>(?:  .*\n|\n)*)", justfile, re.MULTILINE)
    assert match is not None, f"justfile has no recipe {recipe}"
    return match.group("body")


def job_block(workflow: str, job: str) -> str:
    match = re.search(rf"^  {re.escape(job)}:\n(?P<body>.*?)(?=^  [a-zA-Z]|\Z)", workflow, re.MULTILINE | re.DOTALL)
    assert match is not None, f"ci.yml has no job {job}"
    return match.group("body")


class DeferredWiringTests(unittest.TestCase):
    """Every manifest entry must be wired end to end: recipe -> receipts ->
    census -> CI job and matrix -> artifact upload."""

    @classmethod
    def setUpClass(cls) -> None:
        cls.entries = MODULE.deferred_manifest()
        cls.justfile = JUSTFILE.read_text(encoding="utf-8")
        cls.workflow = CI_YML.read_text(encoding="utf-8")

    def test_manifest_entries_exist(self) -> None:
        self.assertTrue(self.entries, "the deferred manifest must not be empty")

    def test_each_entry_is_wired(self) -> None:
        for entry in self.entries:
            if entry["recipe"] == MODULE.PARKED_RECIPE:
                # A parked entry runs nowhere by construction; its ticket and
                # reason are checked by manifest_check.
                continue
            with self.subTest(entry=entry):
                recipe = entry["recipe"]
                body = recipe_block(self.justfile, recipe)
                artifact_dir = str(Path(entry["receipt_artifact"]).parent)
                # The recipe exports LASH_LAW_RECEIPTS under the artifact dir,
                # anchored at the repo root: the test binaries run with the
                # crate dir as cwd, so a relative artifact dir must be
                # prefixed with {{repo}} or the receipts land under
                # crates/lash-restate/target/... and the census reads nothing.
                self.assertIn("export LASH_LAW_RECEIPTS=", body)
                self.assertIn(artifact_dir, body)
                self.assertIn('receipts_dir="{{repo}}/', body)
                # And censuses the deferred laws it ran.
                self.assertIn("--deferred", body)
                self.assertIn(recipe, body.split("--deferred", 1)[1])
                # The CI job's matrix carries the recipe under the entry's
                # matrix name, and an artifact env line names the artifact dir.
                job = job_block(self.workflow, entry["ci_job"])
                matrix_entry = re.search(
                    rf"- name: {re.escape(entry['ci_matrix_name'])}\n"
                    rf"\s*recipe: {re.escape(recipe)}\n",
                    job,
                )
                self.assertIsNotNone(
                    matrix_entry,
                    f"{entry['ci_job']} has no matrix entry "
                    f"{entry['ci_matrix_name']} running {recipe}",
                )
                self.assertRegex(
                    job,
                    rf"LASH_\w*ARTIFACT_DIR: {re.escape(artifact_dir)}\b",
                    f"{entry['ci_job']} sets no artifact-dir env for "
                    f"{artifact_dir}",
                )


if __name__ == "__main__":
    unittest.main()
