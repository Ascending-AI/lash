#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check_conformance_law_registration.py")
SPEC = importlib.util.spec_from_file_location(
    "check_conformance_law_registration", SCRIPT
)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


SUPPORT_INDEX = """\
pub mod runtime_persistence_macro_support {
    pub use super::laws::*;
    pub use crate::conformance::shared::*;
}
"""

MACROS = """\
#[macro_export]
macro_rules! runtime_persistence_tests {
    ($fixture:block) => {
        $crate::runtime_persistence_tests!(@catalogue plain $fixture);
        $crate::runtime_persistence_tests!(@plain_factory $fixture; [(fresh_instances, "fresh")]);
    };
    (@catalogue $mode:ident $fixture:block) => {
        $crate::__runtime_persistence_register! {
            $mode $fixture;
            stores [
            (ordinary_law, "root"),
            ]
            store_refs [
            ]
            factories [
            ]
            timed_stores [
            (timed_law, "root"),
            ]
            timed_factories [
            ]
        }
    };
}

/// Register the shared runtime-persistence laws plus durable reopen laws.
#[macro_export]
macro_rules! runtime_persistence_reopenable_tests {
    ($fixture:block) => {
        $crate::runtime_persistence_tests!(@catalogue reopenable $fixture);
        $crate::runtime_persistence_reopenable_tests!(@reopen_laws $fixture;
            [
                (reopen_law, "root"),
            ]
        );
    };
    (@reopen_laws $fixture:block; [$(( $law:ident, $label:literal )),* $(,)?]) => {
    };
}
"""

LAWS = """\
pub async fn ordinary_law() {
    shared_helper().await;
}
pub async fn timed_law() {}
pub async fn reopen_law() {}
pub async fn fresh_instances() {}

fn private_helper() {}
"""

SHARED = """\
pub async fn shared_helper() {}
"""


def build_tree(root: Path, laws: str = LAWS, macros: str = MACROS) -> None:
    (root / "crates/lash-conformance/src/conformance/runtime_persistence").mkdir(
        parents=True
    )
    (root / "crates/lash-conformance/src/conformance/runtime_persistence/mod.rs").write_text(
        SUPPORT_INDEX, encoding="utf-8"
    )
    (root / "crates/lash-conformance/src/conformance/runtime_persistence/laws.rs").write_text(
        laws, encoding="utf-8"
    )
    (root / "crates/lash-conformance/src/conformance/shared.rs").write_text(
        SHARED, encoding="utf-8"
    )
    (root / "crates/lash-conformance/src/macros.rs").write_text(macros, encoding="utf-8")


class CheckTests(unittest.TestCase):
    def run_check(self, **kwargs: str) -> list[str]:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            build_tree(root, **kwargs)
            return MODULE.check(root)

    def test_clean_tree_passes(self) -> None:
        self.assertEqual(self.run_check(), [])

    def test_unregistered_pub_law_fails_and_names_the_suites(self) -> None:
        laws = LAWS + "\npub async fn forgotten_law() {}\n"
        errors = self.run_check(laws=laws)
        self.assertEqual(len(errors), 1)
        self.assertIn("forgotten_law", errors[0])
        self.assertIn("laws.rs", errors[0])
        self.assertIn("runtime_persistence_tests!", errors[0])
        self.assertIn("@reopen_laws", errors[0])

    def test_catalogue_entry_without_law_fails(self) -> None:
        macros = MACROS.replace("(ordinary_law, ", "(ghost_law, ")
        errors = self.run_check(macros=macros)
        self.assertEqual(len(errors), 2)
        self.assertIn("ghost_law", errors[0])
        self.assertIn("ordinary_law", errors[1])

    def test_reopen_law_also_in_shared_catalogue_fails(self) -> None:
        macros = MACROS.replace(
            '(ordinary_law, "root"),', '(ordinary_law, "root"),\n            (reopen_law, "root"),'
        )
        errors = self.run_check(macros=macros)
        self.assertEqual(len(errors), 1)
        self.assertIn("reopen_law", errors[0])
        self.assertIn("duplicate", errors[0])

    def test_pub_fn_called_by_a_law_is_reachable(self) -> None:
        # `shared_helper` is pub but called by `ordinary_law`: it is a helper,
        # not a law, and must not trip the check.
        self.assertEqual(self.run_check(), [])

    def test_pub_fn_reachable_via_reexport_passes(self) -> None:
        laws = LAWS + "\npub async fn exported_directly() {}\n"
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            build_tree(root, laws=laws)
            index = (
                root
                / "crates/lash-conformance/src/conformance/runtime_persistence/mod.rs"
            )
            index.write_text(
                SUPPORT_INDEX + "\npub use laws::exported_directly;\n",
                encoding="utf-8",
            )
            self.assertEqual(MODULE.check(root), [])


if __name__ == "__main__":
    unittest.main()
