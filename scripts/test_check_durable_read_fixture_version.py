#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
from pathlib import Path
import unittest


SCRIPT = Path(__file__).with_name("check-durable-read-fixture-version.py")
SPEC = importlib.util.spec_from_file_location("durable_read_fixture_version", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(MODULE)


FIXTURE_DIFF = """\
diff --git a/fixtures/durable-read/v1/sqlite/expected.json b/fixtures/durable-read/v1/sqlite/expected.json
--- a/fixtures/durable-read/v1/sqlite/expected.json
+++ b/fixtures/durable-read/v1/sqlite/expected.json
@@ -1 +1 @@
-{}
+{"fixture_schema_version": 2}
"""


class DurableReadFixtureVersionTest(unittest.TestCase):
    def test_artifact_change_without_declaration_fails(self) -> None:
        valid, _ = MODULE.validate_patch(FIXTURE_DIFF)
        self.assertFalse(valid)

    def test_artifact_and_version_change_pass(self) -> None:
        version_diff = """\
diff --git a/crates/lash-core/tests/support/durable_read_fixture.rs b/crates/lash-core/tests/support/durable_read_fixture.rs
--- a/crates/lash-core/tests/support/durable_read_fixture.rs
+++ b/crates/lash-core/tests/support/durable_read_fixture.rs
@@ -1 +1 @@
-pub const DURABLE_READ_FIXTURE_SCHEMA_VERSION: u32 = 1;
+pub const DURABLE_READ_FIXTURE_SCHEMA_VERSION: u32 = 2;
"""
        valid, _ = MODULE.validate_patch(FIXTURE_DIFF + version_diff)
        self.assertTrue(valid)

    def test_readme_only_change_does_not_require_bump(self) -> None:
        readme = FIXTURE_DIFF.replace(
            "v1/sqlite/expected.json", "v1/README.md"
        )
        valid, _ = MODULE.validate_patch(readme)
        self.assertTrue(valid)

    def test_deleted_artifact_requires_bump(self) -> None:
        deletion = FIXTURE_DIFF.replace(
            "+++ b/fixtures/durable-read/v1/sqlite/expected.json", "+++ /dev/null"
        )
        valid, _ = MODULE.validate_patch(deletion)
        self.assertFalse(valid)


if __name__ == "__main__":
    unittest.main()
