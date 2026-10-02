#!/usr/bin/env python3
"""Tests for scripts/check_postgres_gate_coverage.py.

A coverage check that has only ever been seen to pass proves nothing, so
every rule gets a fixture that violates it; the passing fixtures show the
rules do not fire on the shapes the crate legitimately uses.
"""

from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_postgres_gate_coverage as checker  # noqa: E402


STORE_TESTS = """#!/usr/bin/env bash
declare -A uniform_store_suites=(
  [pg-facade-laws]="%s|postgres|lash-runtime|--lib|cargo-test|ignored-only,%s"
  [pg-model-keys]="//crates/lash:llm_profiles__test||lash-runtime|--test llm_profiles|cargo-test|include-ignored"
)
"""

WORKFLOW = """
name: CI
jobs:
  postgres-store:
    runs-on: ubuntu-24.04
    steps:
      - name: Test facade laws on Postgres
        run: |
          bash scripts/ci/with-service.sh "pg16" -- \\
            bash scripts/ci/store-tests.sh pg-facade-laws
      - name: Test model-key laws on Postgres
        run: |
          bash scripts/ci/with-service.sh "pg16" -- \\
            bash scripts/ci/store-tests.sh pg-model-keys
"""

FACADE_LABELS = (
    "//crates/lash:lash__unit_test,"
    "//crates/lash:facade_host_wrappers__test,"
    "//crates/lash:integration__test"
)
SKIPS = "skip=postgres_live_restate,skip=live_postgres"


def postgres_laws(count: int) -> str:
    """Enough PostgreSQL-only laws to clear the scanner's sanity floor."""
    return "\n".join(
        '#[test]\n#[ignore = "requires PostgreSQL"]\nfn postgres_law_%d() {}' % index
        for index in range(count)
    )


class Fixture(unittest.TestCase):
    def setUp(self) -> None:
        self._temp = tempfile.TemporaryDirectory()
        self.addCleanup(self._temp.cleanup)
        self.root = Path(self._temp.name)
        self.write_recipe(labels=FACADE_LABELS, flags="nocapture," + SKIPS)
        self.write_workflow(WORKFLOW)
        (self.root / "crates/lash/src").mkdir(parents=True, exist_ok=True)
        (self.root / "crates/lash/tests").mkdir(parents=True, exist_ok=True)
        self.write_source("src/lib.rs", postgres_laws(checker.MIN_GATE_SITES))

    def write_recipe(self, labels: str, flags: str) -> None:
        path = self.root / "scripts/ci/store-tests.sh"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(STORE_TESTS % (labels, flags), encoding="utf-8")

    def write_workflow(self, body: str) -> None:
        path = self.root / ".github/workflows/ci.yml"
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(body, encoding="utf-8")

    def write_source(self, relative: str, text: str) -> None:
        path = self.root / "crates/lash" / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(text, encoding="utf-8")

    def violations(self) -> list:
        return checker.check_repository(self.root)

    def assert_clean(self) -> None:
        self.assertEqual([], self.violations())


class PassingShapes(Fixture):
    def test_plain_attribute_law_passes(self) -> None:
        self.assert_clean()

    def test_law_table_entry_passes(self) -> None:
        self.write_source(
            "src/tables.rs",
            'laws! {\n  #[ignore = "requires PostgreSQL"]\n  postgres: Storage::Postgres;\n}',
        )
        self.assert_clean()

    def test_macro_argument_gate_passes(self) -> None:
        self.write_source(
            "src/tables.rs",
            'laws!(postgres, true, ignore = "requires the PostgreSQL gate");',
        )
        self.assert_clean()

    def test_bare_reason_literal_passes(self) -> None:
        self.write_source(
            "tests/integration/laws.rs",
            'backend_laws!(double_postgres, Postgres, false, "requires PostgreSQL");',
        )
        self.assert_clean()

    def test_metavariable_generated_law_passes(self) -> None:
        self.write_source(
            "src/gen.rs",
            "macro_rules! laws {\n"
            "  ($mem:ident, $pg:ident) => {\n"
            "    #[test]\n"
            "    fn $mem() {}\n"
            '    #[ignore = "requires PostgreSQL"]\n'
            "    async fn $pg() {}\n"
            "  };\n"
            "}\n"
            "laws! { works_on_sqlite, works_on_postgres }",
        )
        self.assert_clean()

    def test_second_service_law_needs_its_skip(self) -> None:
        self.write_source(
            "src/live.rs",
            '#[test]\n#[ignore = "requires a Restate server and PostgreSQL"]\n'
            "fn postgres_live_restate_law() {}",
        )
        self.assert_clean()

    def test_binary_wide_selection_covers_any_name(self) -> None:
        self.write_source(
            "tests/llm_profiles.rs",
            '#[test]\n#[ignore = "requires PostgreSQL"]\nfn any_name() {}',
        )
        self.assert_clean()


class SelectionDefects(Fixture):
    def test_law_in_an_unselected_binary_fails(self) -> None:
        self.write_source(
            "tests/attachments_evidence.rs",
            '#[test]\n#[ignore = "requires PostgreSQL"]\nfn a_postgres_law() {}',
        )
        violations = self.violations()
        self.assertTrue(
            any("attachments_evidence__test" in v.detail for v in violations),
            violations,
        )

    def test_law_named_without_postgres_fails(self) -> None:
        self.write_source(
            "src/misnamed.rs",
            '#[test]\n#[ignore = "requires PostgreSQL"]\nfn a_durable_law() {}',
        )
        violations = self.violations()
        self.assertTrue(
            any("a_durable_law" in v.detail for v in violations), violations
        )

    def test_second_service_law_run_without_its_skip_fails(self) -> None:
        self.write_recipe(FACADE_LABELS, "nocapture")
        self.write_source(
            "src/live.rs",
            '#[test]\n#[ignore = "requires a Restate server and PostgreSQL"]\n'
            "fn postgres_live_restate_law() {}",
        )
        violations = self.violations()
        self.assertTrue(any("skip=" in v.detail for v in violations), violations)

    def test_suite_missing_from_the_workflow_fails_everything(self) -> None:
        self.write_workflow("name: CI\njobs: {}\n")
        self.assertTrue(self.violations())

    def test_an_unresolvable_marker_fails(self) -> None:
        self.write_source(
            "src/odd.rs",
            '#[ignore = "requires PostgreSQL"]\nconst X: u32 = 1;',
        )
        violations = self.violations()
        self.assertTrue(
            any("cannot resolve" in v.detail for v in violations), violations
        )

    def test_a_bare_scanner_floor_fails(self) -> None:
        (self.root / "crates/lash/src/lib.rs").write_text("", encoding="utf-8")
        violations = self.violations()
        self.assertTrue(
            any("PostgreSQL-gated ignored sites" in v.detail for v in violations),
            violations,
        )


if __name__ == "__main__":
    unittest.main()
