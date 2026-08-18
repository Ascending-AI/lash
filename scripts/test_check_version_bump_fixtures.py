#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("check_version_bump_fixtures.py")
SPEC = importlib.util.spec_from_file_location("check_version_bump_fixtures", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


VERSION_SOURCE = "const SCHEMA_VERSION: i32 = 52;\n"

MIGRATIONS_SOURCE = """\
const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[
    SchemaMigration {
        from: 51,
        to: 52,
        source_missing_tables: &["lash_fence"],
        source_missing_columns: &[],
        source_missing_guards: &[],
        introduced_relations: &["lash_fence"],
        statements: &[
            FENCE_DDL,
            r#"UPDATE lash_schema_versions
               SET version = 52
             WHERE component = 'lash-postgres-store' AND version = 51"#,
        ],
    },
    SchemaMigration {
        from: 50,
        to: 52,
        source_missing_tables: &["lash_fence", "lash_plans"],
        source_missing_columns: &[("lash_sessions", "enqueued_at_ms")],
        source_missing_guards: &[DeclaredGuard {
            table: "lash_sessions",
            columns: &["session_id", "enqueued_at_ms"],
            predicate: "(enqueued_at_ms is not null)",
        }],
        introduced_relations: &[
            "lash_fence",
            "lash_plans",
            "idx_lash_plans",
            "idx_lash_sessions_order",
        ],
        statements: &[PLANS_DDL, PLANS_INDEX_DDL, SESSIONS_ORDER_INDEX_DDL, FENCE_DDL],
    },
];

const PLANS_INDEX_DDL: &str = r#"CREATE INDEX idx_lash_plans
            ON lash_plans(session_id)"#;

const SESSIONS_ORDER_INDEX_DDL: &str = r#"CREATE INDEX idx_lash_sessions_order
            ON lash_sessions(session_id, enqueued_at_ms)"#;

fn schema_migration_divergence_error(found: i32) -> StoreError {
    StoreError::Backend(format!(
        "component has version {found} but contains schema artifacts newer than the \\
         recorded version: {}. Inspect and recreate."
    ))
}

fn schema_migration_source_mismatch_error(found: i32) -> StoreError {
    StoreError::Backend(format!(
        "component has version {found} but the live schema does not match the published \\
         component-{found} migration source shape."
    ))
}

pub(crate) fn version_mismatch_error(found: Option<i32>) -> StoreError {
    StoreError::Backend(format!(
        "component is at {found:?}. This mismatch \\
         has no applicable migration. Recreate the trust domain."
    ))
}
"""

FIXTURE_SOURCE = """\
const MIGRATION_FLOOR_VERSION: i32 = 50;
const POST_FLOOR_TABLES: [&str; 2] = ["lash_fence", "lash_plans"];
const POST_FLOOR_INDEXES: [&str; 1] = ["idx_lash_sessions_order"];
const POST_FLOOR_COLUMNS: [(&str, &str); 1] = [("lash_sessions", "enqueued_at_ms")];
const POST_FLOOR_ARTIFACTS: [&str; 4] = [
    "idx_lash_plans",
    "idx_lash_sessions_order",
    "lash_fence",
    "lash_plans",
];
const DIVERGENT_ARTIFACTS: [&str; 1] = ["lash_fence"];
const DIVERGENT_ARTIFACTS_MARKER: &str = "schema artifacts newer than the recorded version";
const SOURCE_MISMATCH_MARKER: &str = "does not match the published component-";
const NO_APPLICABLE_MIGRATION_MARKER: &str = "has no applicable migration";

impl RefusalKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::DivergentArtifacts => "divergent_artifacts",
            Self::MigrationSourceMismatch => "migration_source_mismatch",
            Self::NoApplicableMigration => "no_applicable_migration",
        }
    }
}
"""

GATE_SOURCE = """\
python3 - "$artifact_dir" <<'PY'
EXPECTED_REFUSAL_KINDS = {
    "refused_divergent_store": "divergent_artifacts",
    "refused_older_store": "no_applicable_migration",
    "refused_newer_store": "no_applicable_migration",
    "recreated_store": "no_applicable_migration",
}
PY
"""


class VersionBumpFixtureCheckTest(unittest.TestCase):
    def check(
        self,
        *,
        version: str = VERSION_SOURCE,
        migrations: str = MIGRATIONS_SOURCE,
        fixture: str = FIXTURE_SOURCE,
        gate: str = GATE_SOURCE,
    ) -> tuple[bool, str]:
        with tempfile.TemporaryDirectory() as directory:
            repo = Path(directory)
            for relative, text in (
                (MODULE.VERSION_SOURCE, version),
                (MODULE.MIGRATIONS_SOURCE, migrations),
                (MODULE.FIXTURE_SOURCE, fixture),
                (MODULE.GATE_SOURCE, gate),
            ):
                path = repo / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text, encoding="utf-8")
            return MODULE.check(repo)

    def test_coherent_fixtures_pass(self) -> None:
        valid, message = self.check()
        self.assertTrue(valid, message)
        self.assertIn("component 52, floor 50", message)
        self.assertIn("1 explicitly dropped post-floor columns", message)

    def test_stale_floor_fails(self) -> None:
        valid, message = self.check(
            fixture=FIXTURE_SOURCE.replace(
                "MIGRATION_FLOOR_VERSION: i32 = 50", "MIGRATION_FLOOR_VERSION: i32 = 51"
            )
        )
        self.assertFalse(valid)
        self.assertIn("MIGRATION_FLOOR_VERSION is 51", message)
        # The FIG-1259 wrong-refusal cause: the stamp one below a stale floor is a
        # version this build migrates rather than refuses.
        self.assertIn("would be migrated instead of refused", message)

    def test_stale_post_floor_lists_fail(self) -> None:
        valid, message = self.check(
            fixture=FIXTURE_SOURCE.replace(
                'const POST_FLOOR_TABLES: [&str; 2] = ["lash_fence", "lash_plans"];',
                'const POST_FLOOR_TABLES: [&str; 1] = ["lash_plans"];',
            )
        )
        self.assertFalse(valid)
        self.assertIn("POST_FLOOR_TABLES is not", message)
        self.assertIn("missing lash_fence", message)

    def test_post_floor_index_the_table_drops_miss_must_be_listed(self) -> None:
        # An index over a table the floor catalog already had survives every
        # `DROP TABLE`, so omitting it leaves a current-only artifact — the failure
        # the container gate reports as `current_artifact_count != 0`.
        valid, message = self.check(
            fixture=FIXTURE_SOURCE.replace(
                'const POST_FLOOR_INDEXES: [&str; 1] = ["idx_lash_sessions_order"];',
                "const POST_FLOOR_INDEXES: [&str; 0] = [];",
            )
        )
        self.assertFalse(valid)
        self.assertIn("POST_FLOOR_INDEXES is not", message)
        self.assertIn("missing idx_lash_sessions_order", message)

    def test_post_floor_index_on_a_dropped_table_is_rejected(self) -> None:
        # `DROP TABLE` already took this one, so naming it would drop a relation
        # that no longer exists.
        valid, message = self.check(
            fixture=FIXTURE_SOURCE.replace(
                'const POST_FLOOR_INDEXES: [&str; 1] = ["idx_lash_sessions_order"];',
                'const POST_FLOOR_INDEXES: [&str; 2] = ["idx_lash_sessions_order", '
                '"idx_lash_plans"];',
            )
        )
        self.assertFalse(valid)
        self.assertIn("POST_FLOOR_INDEXES is not", message)
        self.assertIn("stale idx_lash_plans", message)

    def test_post_floor_column_the_table_drops_miss_must_be_listed(self) -> None:
        # A nullable column added to a table the floor catalog already had survives
        # every `DROP TABLE` and every `DROP INDEX`, so omitting it leaves the
        # older-store fixture carrying a current-generation column: the refusal it
        # then proves is not the one a genuinely older store gets.
        valid, message = self.check(
            fixture=FIXTURE_SOURCE.replace(
                'const POST_FLOOR_COLUMNS: [(&str, &str); 1] = '
                '[("lash_sessions", "enqueued_at_ms")];',
                "const POST_FLOOR_COLUMNS: [(&str, &str); 0] = [];",
            )
        )
        self.assertFalse(valid)
        self.assertIn("POST_FLOOR_COLUMNS is not", message)
        self.assertIn("missing lash_sessions.enqueued_at_ms", message)

    def test_post_floor_column_naming_a_column_the_floor_never_lost_is_rejected(
        self,
    ) -> None:
        # The mirror failure: dropping a column the published floor catalog never
        # had is a `DROP COLUMN` of something that is not there, which reds the
        # container run rather than the fixture check.
        valid, message = self.check(
            fixture=FIXTURE_SOURCE.replace(
                'const POST_FLOOR_COLUMNS: [(&str, &str); 1] = '
                '[("lash_sessions", "enqueued_at_ms")];',
                'const POST_FLOOR_COLUMNS: [(&str, &str); 2] = ['
                '("lash_sessions", "enqueued_at_ms"), ("lash_sessions", "invented")];',
            )
        )
        self.assertFalse(valid)
        self.assertIn("POST_FLOOR_COLUMNS is not", message)
        self.assertIn("stale lash_sessions.invented", message)

    def test_a_migration_entry_without_a_column_axis_is_refused(self) -> None:
        # The field is required rather than defaulted: a migration that adds a
        # column and forgets to declare it would silently leave the rewind with no
        # column to drop, which is the whole defect this axis exists for.
        with self.assertRaises(MODULE.CheckError) as raised:
            self.check(
                migrations=MIGRATIONS_SOURCE.replace(
                    'source_missing_columns: &[("lash_sessions", "enqueued_at_ms")],\n',
                    "",
                )
            )
        self.assertIn("no source_missing_columns", str(raised.exception))

    def test_a_column_axis_that_is_not_pairs_is_refused(self) -> None:
        # `(table, column)` is the shape the derivation reads. A bare list of names
        # would parse as half as many pairs, silently shrinking the set.
        with self.assertRaises(MODULE.CheckError) as raised:
            self.check(
                migrations=MIGRATIONS_SOURCE.replace(
                    '&[("lash_sessions", "enqueued_at_ms")]',
                    '&["lash_sessions", "enqueued_at_ms", "extra"]',
                )
            )
        self.assertIn("does not list (table, column) pairs", str(raised.exception))

    def test_ambiguous_index_target_cannot_be_decided(self) -> None:
        # Last-match-wins would let the second occurrence — on a table the floor
        # drops — excuse the index from POST_FLOOR_INDEXES, so the omission would
        # only surface as `current_artifact_count != 0` inside the container.
        with self.assertRaises(MODULE.CheckError) as raised:
            self.check(
                migrations=MIGRATIONS_SOURCE
                + 'const REBUILT_DDL: &str = r#"CREATE INDEX idx_lash_sessions_order\n'
                '            ON lash_plans(session_id)"#;\n'
            )
        self.assertIn("idx_lash_sessions_order", str(raised.exception))
        self.assertIn("over more than one table", str(raised.exception))

    def test_commented_out_ddl_is_not_read_as_ddl(self) -> None:
        # The same text as prose. A doc comment describing DDL executes nothing, so
        # it must not enter the index map — where it would collide with the real
        # statement and stall the check.
        valid, message = self.check(
            migrations=MIGRATIONS_SOURCE
            + "/// CREATE INDEX idx_lash_sessions_order\n"
            "///             ON lash_plans(session_id)\n"
            "// CREATE INDEX idx_lash_plans ON lash_sessions(session_id)\n"
        )
        self.assertTrue(valid, message)

    def test_index_target_reads_concurrently_and_only(self) -> None:
        # `CONCURRENTLY` and `ONLY` are ordinary PostgreSQL index DDL. Failing to
        # read either leaves the index with no resolved table, which would demand
        # a POST_FLOOR_INDEXES entry for a relation `DROP TABLE lash_plans`
        # already takes.
        valid, message = self.check(
            migrations=MIGRATIONS_SOURCE.replace(
                "CREATE INDEX idx_lash_plans\n            ON lash_plans(session_id)",
                "CREATE INDEX CONCURRENTLY IF NOT EXISTS idx_lash_plans\n"
                "            ON ONLY lash_plans(session_id)",
            )
        )
        self.assertTrue(valid, message)

    def test_stale_divergent_artifacts_fail(self) -> None:
        valid, message = self.check(
            fixture=FIXTURE_SOURCE.replace(
                'const DIVERGENT_ARTIFACTS: [&str; 1] = ["lash_fence"];',
                'const DIVERGENT_ARTIFACTS: [&str; 1] = ["lash_plans"];',
            )
        )
        self.assertFalse(valid)
        self.assertIn("DIVERGENT_ARTIFACTS is not", message)
        self.assertIn("stale lash_plans", message)

    def test_bump_without_a_predecessor_migration_fails(self) -> None:
        valid, message = self.check(version="const SCHEMA_VERSION: i32 = 53;\n")
        self.assertFalse(valid)
        self.assertIn("not the current component version 53", message)

    def test_predecessor_generation_must_be_migratable(self) -> None:
        valid, message = self.check(
            version="const SCHEMA_VERSION: i32 = 52;\n",
            migrations=MIGRATIONS_SOURCE.replace("from: 51,", "from: 49,").replace(
                "AND version = 51", "AND version = 49"
            ),
        )
        self.assertFalse(valid)
        self.assertIn("no migration from component 51", message)

    def test_missing_refusal_marker_fails(self) -> None:
        valid, message = self.check(
            migrations=MIGRATIONS_SOURCE.replace(
                "has no applicable migration", "cannot be migrated"
            )
        )
        self.assertFalse(valid)
        self.assertIn("NO_APPLICABLE_MIGRATION_MARKER", message)
        self.assertIn("no longer appears", message)

    def test_overlapping_refusal_markers_fail(self) -> None:
        # Presence alone is not enough: a marker carried by a sibling renderer
        # makes two kinds match at once, which the harness rejects mid-run.
        valid, message = self.check(
            migrations=MIGRATIONS_SOURCE.replace(
                "recorded version: {}. Inspect and recreate.",
                "recorded version: {}. This mismatch has no applicable migration.",
            )
        )
        self.assertFalse(valid)
        self.assertIn("must select only version_mismatch_error", message)
        self.assertIn("schema_migration_divergence_error", message)

    def test_marker_selecting_the_wrong_renderer_fails(self) -> None:
        valid, message = self.check(
            fixture=FIXTURE_SOURCE.replace(
                'const DIVERGENT_ARTIFACTS_MARKER: &str = "schema artifacts newer than '
                'the recorded version";',
                'const DIVERGENT_ARTIFACTS_MARKER: &str = "migration source shape";',
            )
        )
        self.assertFalse(valid)
        self.assertIn("must select only schema_migration_divergence_error", message)
        self.assertIn("schema_migration_source_mismatch_error", message)

    def test_renamed_kind_literal_fails(self) -> None:
        valid, message = self.check(
            fixture=FIXTURE_SOURCE.replace(
                'Self::DivergentArtifacts => "divergent_artifacts",',
                'Self::DivergentArtifacts => "divergence",',
            )
        )
        self.assertFalse(valid)
        self.assertIn("demands 'divergent_artifacts'", message)
        self.assertIn("no RefusalKind variant", message)

    def test_gate_demanding_an_unemitted_kind_fails(self) -> None:
        valid, message = self.check(
            gate=GATE_SOURCE.replace(
                '"refused_older_store": "no_applicable_migration",',
                '"refused_older_store": "reject_and_recreate",',
            )
        )
        self.assertFalse(valid)
        self.assertIn("demands 'reject_and_recreate'", message)

    def test_unparseable_sources_are_undecided(self) -> None:
        with self.assertRaises(MODULE.CheckError):
            self.check(migrations="const SCHEMA_MIGRATIONS: &[SchemaMigration] = &[];\n")
        with self.assertRaises(MODULE.CheckError):
            self.check(fixture=FIXTURE_SOURCE.replace("[&str; 2]", "[&str; 9]"))
        with self.assertRaises(MODULE.CheckError):
            self.check(version="// no version here\n")
        # A renderer the classifier must select cannot simply vanish.
        with self.assertRaises(MODULE.CheckError):
            self.check(
                migrations=MIGRATIONS_SOURCE.replace(
                    "pub(crate) fn version_mismatch_error", "fn renamed_error"
                )
            )
        # Nor can the two kind tables lose their parseable shape.
        with self.assertRaises(MODULE.CheckError):
            self.check(fixture=FIXTURE_SOURCE.replace("Self::", "RefusalKind::"))
        with self.assertRaises(MODULE.CheckError):
            self.check(gate=GATE_SOURCE.replace("EXPECTED_REFUSAL_KINDS", "KINDS"))
        # Two variants may not share one wire string.
        with self.assertRaises(MODULE.CheckError):
            self.check(
                fixture=FIXTURE_SOURCE.replace(
                    'Self::MigrationSourceMismatch => "migration_source_mismatch",',
                    'Self::MigrationSourceMismatch => "divergent_artifacts",',
                )
            )


if __name__ == "__main__":
    unittest.main()
