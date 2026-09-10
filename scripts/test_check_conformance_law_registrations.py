#!/usr/bin/env python3
"""Tests for the runtime persistence law registration gate."""

from __future__ import annotations

from pathlib import Path
import importlib.util
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check_conformance_law_registrations.py")
SPEC = importlib.util.spec_from_file_location("law_registration_check", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
CHECKER = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = CHECKER
SPEC.loader.exec_module(CHECKER)

INVENTORY = """\
pub async fn runtime_persistence<F>(law: RuntimePersistenceLaw) {
    if matches!(law, RuntimePersistenceLaw::fresh_instances) {}
}
/// Run one independent durable reopen or runtime persistence vector.
pub async fn runtime_persistence_reopenable<F>(law: RuntimePersistenceLaw) {
    match law {
        RuntimePersistenceLaw::reopen_mint_identity => {}
        _ => {}
    }
}
pub(super) fn assert_two_session_resolution_errors() {}
pub enum RuntimePersistenceLaw {
    alpha,
    fresh_instances,
    reopen_mint_identity,
}
"""
MACROS = """\
macro_rules! runtime_persistence_tests {
    ($runner:ident) => {
        async fn alpha() { $runner($crate::RuntimePersistenceLaw::alpha).await; }
        async fn fresh_instances() { $runner($crate::RuntimePersistenceLaw::fresh_instances).await; }
    };
}
macro_rules! runtime_persistence_reopenable_tests {
    ($runner:ident) => {
        async fn alpha() { $runner($crate::RuntimePersistenceLaw::alpha).await; }
        async fn reopen_mint_identity() { $runner($crate::RuntimePersistenceLaw::reopen_mint_identity).await; }
    };
}
"""


def fixture(root: Path) -> None:
    enum_path = root / CHECKER.ENUM
    enum_path.parent.mkdir(parents=True, exist_ok=True)
    enum_path.write_text(INVENTORY, encoding="utf-8")
    macro_path = root / CHECKER.MACROS
    macro_path.parent.mkdir(parents=True, exist_ok=True)
    macro_path.write_text(MACROS, encoding="utf-8")
    for _, path, macro in CHECKER.SITES:
        target = root / path
        target.parent.mkdir(parents=True, exist_ok=True)
        target.write_text(f"{macro}!(runner);\n", encoding="utf-8")


class LawRegistrationTests(unittest.TestCase):
    def test_repository_passes(self) -> None:
        self.assertEqual(CHECKER.check_repository(CHECKER.ROOT), [])

    def test_runner_split_is_source_derived(self) -> None:
        laws = set(CHECKER.enum_laws(INVENTORY))
        self.assertEqual(laws - CHECKER.runner_special_laws(INVENTORY, reopenable=False), {"alpha", "reopen_mint_identity"})
        self.assertEqual(laws - CHECKER.runner_special_laws(INVENTORY, reopenable=True), {"alpha", "fresh_instances"})

    def test_malformed_source_fails_closed(self) -> None:
        with self.assertRaisesRegex(ValueError, "unrecognized RuntimePersistenceLaw entry"):
            CHECKER.enum_laws(INVENTORY.replace("    alpha,", "    alpha {"))
        malformed = MACROS.replace(
            "async fn alpha() { $runner($crate::RuntimePersistenceLaw::alpha).await; }",
            "async fn alpha() { unsupported_registration; }",
        )
        with self.assertRaisesRegex(ValueError, "unrecognized runtime_persistence_tests"):
            CHECKER.macro_laws(malformed, "runtime_persistence_tests")

    def test_missing_and_equal_count_wrong_identity_are_named(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture(root)
            path = root / CHECKER.MACROS
            path.write_text(
                MACROS.replace(
                    "async fn fresh_instances() { $runner($crate::RuntimePersistenceLaw::fresh_instances).await; }",
                    "async fn fresh_instances() { $runner($crate::RuntimePersistenceLaw::alpha).await; }",
                ),
                encoding="utf-8",
            )
            errors = CHECKER.check_repository(root)
        self.assertTrue(any("duplicate law identities: alpha" in error for error in errors), errors)
        self.assertTrue(any("missing law identities: fresh_instances" in error for error in errors), errors)
        self.assertTrue(any("test/law identity mismatches: fresh_instances" in error for error in errors), errors)

    def test_whole_backend_registration_removal_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            fixture(root)
            (root / CHECKER.SITES[1][1]).write_text("// registration removed\n", encoding="utf-8")
            errors = CHECKER.check_repository(root)
        self.assertTrue(any("SQLite registration site must contain exactly one" in error for error in errors), errors)

    def test_commented_or_disabled_backend_registration_fails_closed(self) -> None:
        for source, expected in (
            ("// runtime_persistence_reopenable_tests!(runner);\n", "exactly one"),
            ("/* runtime_persistence_reopenable_tests!(runner); */\n", "exactly one"),
            ("#[cfg(any())]\nruntime_persistence_reopenable_tests!(runner);\n", "disables"),
        ):
            with self.subTest(source=source):
                with tempfile.TemporaryDirectory() as directory:
                    root = Path(directory)
                    fixture(root)
                    (root / CHECKER.SITES[1][1]).write_text(source, encoding="utf-8")
                    errors = CHECKER.check_repository(root)
                self.assertTrue(any("SQLite registration site" in error and expected in error for error in errors), errors)


if __name__ == "__main__":
    unittest.main()
