#!/usr/bin/env python3
"""Fixture tests for check_outcome_suffixes.py.

Each test builds a miniature workspace — `.rs` files under `crates/` — and
runs the gate's `violations()` against it. The nested `pub use` and
non-facade cases are the surface the pre-FIG-4197 gate could not see: it
read `pub use` trees only in `crates/lash/src/lib.rs` and `pub` items only
under `crates/lash/src/` and `crates/lash-remote-protocol/src/`.
"""

from __future__ import annotations

import contextlib
import io
from pathlib import Path
import tempfile
import unittest

import check_outcome_suffixes as gate


class FixtureWorkspace:
    def __init__(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.repo = Path(self.temporary.name)

    def close(self) -> None:
        self.temporary.cleanup()

    def write(self, relative: str, content: str) -> Path:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")
        return path


class OutcomeSuffixGateTests(unittest.TestCase):
    def setUp(self) -> None:
        self.fixture = FixtureWorkspace()
        self.addCleanup(self.fixture.close)

    def violations(self) -> dict[str, list[str]]:
        return gate.violations(self.fixture.repo)

    def test_retired_suffix_declaration_is_flagged(self) -> None:
        self.fixture.write(
            "crates/lash/src/lib.rs", "pub struct LedgerSummary;\n"
        )
        self.assertIn("LedgerSummary", self.violations())

    def test_nested_pub_use_reexport_is_flagged(self) -> None:
        # A `pub use` inside a submodule mints the retired name a host writes;
        # the old gate read `pub use` trees only in the facade root, so this
        # case stayed red before the widening.
        self.fixture.write("crates/lash/src/lib.rs", "pub mod usage;\n")
        self.fixture.write(
            "crates/lash/src/usage.rs",
            "pub use crate::inner::LedgerView as LedgerSummary;\n",
        )
        self.fixture.write("crates/lash/src/inner.rs", "pub struct LedgerView;\n")

        found = self.violations()
        self.assertIn("LedgerSummary", found)
        self.assertEqual(
            ["crates/lash/src/usage.rs:1"], found["LedgerSummary"]
        )
        self.assertNotIn("LedgerView", found)

    def test_non_facade_crate_is_scanned(self) -> None:
        # The old gate read `crates/lash` and `crates/lash-remote-protocol`
        # alone; a retired suffix in any other workspace crate escaped it.
        self.fixture.write(
            "crates/lash-sim/src/lib.rs", "pub enum SimDisposition { Done }\n"
        )
        self.assertIn("SimDisposition", self.violations())

    def test_glob_reexport_reaches_the_declaration(self) -> None:
        self.fixture.write(
            "crates/lash-core/src/lib.rs",
            "pub mod inner {\n    pub use self::deeper::*;\n}\n",
        )
        self.fixture.write(
            "crates/lash-core/src/inner/deeper.rs", "pub struct HiddenResult;\n"
        )
        self.assertIn("HiddenResult", self.violations())

    def test_cfg_test_inline_module_is_exempt(self) -> None:
        self.fixture.write(
            "crates/lash-core/src/lib.rs",
            "pub struct Clean;\n\n#[cfg(test)]\nmod tests {\n"
            "    pub struct FakeSummary;\n"
            "    pub fn f() { let x = 1; }\n"
            "}\n",
        )
        self.assertEqual({}, self.violations())

    def test_cfg_test_file_module_is_exempt(self) -> None:
        self.fixture.write(
            "crates/lash-core/src/lib.rs", "#[cfg(test)]\nmod helpers;\n"
        )
        self.fixture.write(
            "crates/lash-core/src/helpers.rs",
            "pub struct TestOnlyDisposition;\n",
        )
        self.assertEqual({}, self.violations())

    def test_tests_directory_is_exempt(self) -> None:
        self.fixture.write(
            "crates/lash-core/tests/integration.rs",
            "pub struct HarnessSummary;\n",
        )
        self.assertEqual({}, self.violations())

    def test_cfg_not_test_is_not_exempt(self) -> None:
        self.fixture.write(
            "crates/lash-core/src/lib.rs",
            "#[cfg(not(test))]\npub struct ShippingSummary;\n",
        )
        self.assertIn("ShippingSummary", self.violations())

    def test_result_aliases_pass(self) -> None:
        self.fixture.write(
            "crates/lash/src/lib.rs",
            "pub type Result<T> = std::result::Result<T, E>;\n"
            "pub struct E;\n"
            "pub type MaintenanceResult<R> = Result<R, MaintenanceFailure<R>>;\n"
            "pub struct MaintenanceFailure<R>;\n",
        )
        self.assertEqual({}, self.violations())

    def test_as_alias_leaf_is_flagged(self) -> None:
        self.fixture.write(
            "crates/lash-plugin-mcp/src/lib.rs",
            "pub use rmcp::model::{Clean, SpecResult as FixtureSummary};\n",
        )
        found = self.violations()
        self.assertIn("FixtureSummary", found)
        self.assertNotIn("Clean", found)

    def test_unparsed_use_tree_fails_loud(self) -> None:
        self.fixture.write(
            "crates/lash/src/lib.rs", "pub use a::<odd>;\n"
        )
        self.assertTrue(
            any(name.startswith("<unparsed") for name in self.violations())
        )

    def test_clean_workspace_passes(self) -> None:
        self.fixture.write(
            "crates/lash/src/lib.rs",
            "pub struct TurnOutcome;\npub use crate::inner::Clean;\n",
        )
        self.fixture.write("crates/lash/src/inner.rs", "pub struct Clean;\n")
        with contextlib.redirect_stdout(io.StringIO()):
            self.assertEqual(0, gate.main(self.fixture.repo))

    def test_doc_comment_item_is_not_code(self) -> None:
        self.fixture.write(
            "crates/lash/src/lib.rs",
            '/// ```\n/// pub struct DocSummary;\n/// ```\npub struct Clean;\n',
        )
        self.assertEqual({}, self.violations())

    def test_real_workspace_is_clean(self) -> None:
        self.assertEqual({}, gate.violations(gate.REPO))


if __name__ == "__main__":
    unittest.main()
