#!/usr/bin/env python3
"""Fixture tests for check_version_bumps.py.

Each fixture is a throwaway git repository with a two-surface registry: a
decoder-law ``migrate`` surface and a ``coexist`` wire. The four verdicts the
1.0 cut records (FIG-4494) are the first four tests, each run as the command
CI runs.
"""

from __future__ import annotations

import contextlib
import importlib.util
import io
from pathlib import Path
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest


SCRIPT = Path(__file__).with_name("check_version_bumps.py")
SPEC = importlib.util.spec_from_file_location("check_version_bumps", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)


REGISTRY = """
[[surface]]
constant = "WIRE_VERSION"
constant_path = "crates/demo/src/lib.rs"
upgrade = "migrate"
description = "fixture stored record"

[[surface]]
constant = "PEER_PROTOCOL_VERSION"
constant_path = "crates/demo/src/peer.rs"
upgrade = "coexist"
description = "fixture wire protocol"
"""

LIB = """
/// The stored record's format.
#[cfg(not(feature = "synthetic-next"))]
/// version_guard(items(Record, encode_record))
pub const WIRE_VERSION: u32 = 3;

/// The upgrade harness's stand-in successor.
#[cfg(feature = "synthetic-next")]
pub const WIRE_VERSION: u32 = 4;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
}

pub fn encode_record(record: &Record) -> Vec<u8> {
    record.id.as_bytes().to_vec()
}
"""

PEER = """
/// version_guard(shapes(path = "crates/demo/src/dto/*.rs", cover(Hello)))
pub const PEER_PROTOCOL_VERSION: u32 = 1;
"""

DTO = """
#[derive(Serialize, Deserialize)]
pub struct Hello {
    pub name: String,
}
"""

LIFTS = """
#[cfg(not(feature = "synthetic-next"))]
pub const RECORD_UPCASTERS: &[RecordUpcaster] = &[];

#[cfg(feature = "synthetic-next")]
pub const RECORD_UPCASTERS: &[RecordUpcaster] = super::synthetic_next::RECORD_UPCASTERS;
"""

LIFT_FROM_3 = """
#[cfg(not(feature = "synthetic-next"))]
pub const RECORD_UPCASTERS: &[RecordUpcaster] = &[
    RecordUpcaster {
        constant: "WIRE_VERSION",
        from_version: 3,
        lift: Lift::Json(lift_record_3),
    },
];

#[cfg(feature = "synthetic-next")]
pub const RECORD_UPCASTERS: &[RecordUpcaster] = super::synthetic_next::RECORD_UPCASTERS;
"""

WIDER_RECORD = LIB.replace("pub id: String,", "pub id: String,\n    pub owner: String,")

DDL_SURFACE = """
[[surface]]
constant = "SCHEMA_VERSION"
constant_path = "crates/demo/src/store.rs"
upgrade = "migrate"
unguarded = "catalog DDL stamp"
description = "fixture store DDL"
"""

STORE = """
/// version_guard(
///     file(path = "crates/demo/schema.sql", cover("CREATE TABLE demo")),
///     catalog(path = "crates/demo/src/migrate.rs", MIGRATIONS),
/// )
const SCHEMA_VERSION: i32 = 7;
"""

SCHEMA = "CREATE TABLE demo (id TEXT);\n"
WIDER_SCHEMA = "CREATE TABLE demo (id TEXT, note TEXT);\n"

# The one shipped step ends at the base version. Its statement text and the
# synthetic-next row both spell a step from 7, and neither is one.
MIGRATIONS = """
static MIGRATIONS: &[Migration] = &[
    Migration {
        id: "0007-demo",
        from_version: 6,
        to_version: 7,
        statements: "CREATE TABLE demo (id TEXT); -- from_version: 7, to_version: 8,",
    },
    #[cfg(feature = "synthetic-next")]
    Migration {
        id: "synthetic-next",
        from_version: 7,
        to_version: 8,
        statements: SYNTHETIC_NEXT_DDL,
    },
];
"""

STEP_FROM_7 = """    Migration {
        id: "0008-note",
        from_version: 7,
        to_version: 8,
        statements: "ALTER TABLE demo ADD COLUMN note TEXT",
    },
    #[cfg(feature = "synthetic-next")]
"""


class Fixture(unittest.TestCase):
    def setUp(self) -> None:
        self._tmp = tempfile.TemporaryDirectory()
        self.repo = Path(self._tmp.name)
        self.git("init", "--quiet", "--initial-branch=main")
        self.write(gate.REGISTRY, REGISTRY)
        self.write("crates/demo/src/lib.rs", LIB)
        self.write("crates/demo/src/peer.rs", PEER)
        self.write("crates/demo/src/dto/hello.rs", DTO)
        self.write(gate.UPCASTER_REGISTRY, LIFTS)
        self.base = self.commit("base")

    def tearDown(self) -> None:
        self._tmp.cleanup()

    def git(self, *args: str) -> str:
        return subprocess.run(
            ["git", "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
             "-c", "commit.gpgsign=false", *args],
            cwd=self.repo,
            check=True,
            capture_output=True,
            text=True,
        ).stdout.strip()

    def write(self, relative: str, text: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text), encoding="utf-8")

    def commit(self, message: str) -> str:
        self.git("add", "--all")
        self.git("commit", "--quiet", "--allow-empty", "--message", message)
        return self.git("rev-parse", "HEAD")

    def run_command(self, base: str, head: str) -> subprocess.CompletedProcess[str]:
        """The gate exactly as CI invokes it."""
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--base", base, "--head", head,
             "--repo", str(self.repo)],
            capture_output=True,
            text=True,
        )

    def verdict(self, head: str, base: str | None = None) -> tuple[int, str]:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = gate.main(
                ["--base", base or self.base, "--head", head, "--repo", str(self.repo)]
            )
        return code, output.getvalue()


class CutVerdicts(Fixture):
    """The four machine verdicts FIG-4494 records."""

    def test_an_unchanged_candidate_exits_zero(self) -> None:
        result = self.run_command(self.base, self.base)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("2 of 2 surfaces evaluated", result.stdout)
        self.assertIn("0 not evaluated", result.stdout)

    def test_a_changed_shape_with_its_bump_and_upgrade_evidence_exits_zero(self) -> None:
        self.write("crates/demo/src/lib.rs", WIDER_RECORD.replace("u32 = 3;", "u32 = 4;", 1))
        self.write(gate.UPCASTER_REGISTRY, LIFT_FROM_3)
        result = self.run_command(self.base, self.commit("bump with its lift"))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("WIRE_VERSION 3 to 4 (lift rows registered)", result.stdout)

    def test_the_same_change_without_the_bump_exits_nonzero(self) -> None:
        self.write("crates/demo/src/lib.rs", WIDER_RECORD)
        self.write(gate.UPCASTER_REGISTRY, LIFT_FROM_3)
        result = self.run_command(self.base, self.commit("no bump"))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("WIRE_VERSION is 3 on both sides", result.stderr)
        self.assertIn("Bump WIRE_VERSION strictly past 3", result.stderr)

    def test_a_malformed_guard_exits_nonzero(self) -> None:
        self.write(
            "crates/demo/src/lib.rs",
            LIB.replace("items(Record, encode_record)", "items(Record, encode_record"),
        )
        result = self.run_command(self.base, self.commit("malformed marker"))
        self.assertEqual(result.returncode, 2, result.stdout + result.stderr)
        self.assertIn("could not evaluate", result.stderr)
        self.assertIn("crates/demo/src/lib.rs:WIRE_VERSION", result.stderr)
        self.assertIn("1 of 2 surfaces evaluated", result.stderr)


class BumpAndEvidence(Fixture):
    def test_a_decoder_law_bump_without_its_lift_fails(self) -> None:
        self.write("crates/demo/src/lib.rs", WIDER_RECORD.replace("u32 = 3;", "u32 = 4;", 1))
        code, output = self.verdict(self.commit("bump, no lift"))
        self.assertEqual(code, 1, output)
        self.assertIn("without its upgrade evidence", output)
        self.assertIn("lifting from version 3", output)

    def test_a_bump_over_two_versions_owes_both_lifts(self) -> None:
        self.write("crates/demo/src/lib.rs", WIDER_RECORD.replace("u32 = 3;", "u32 = 5;", 1))
        self.write(gate.UPCASTER_REGISTRY, LIFT_FROM_3)
        code, output = self.verdict(self.commit("skips 4"))
        self.assertEqual(code, 1, output)
        self.assertIn("lifting from version 4", output)

    def test_a_lift_table_that_cannot_be_read_is_an_error(self) -> None:
        self.write("crates/demo/src/lib.rs", WIDER_RECORD.replace("u32 = 3;", "u32 = 4;", 1))
        self.write(
            gate.UPCASTER_REGISTRY,
            "pub const RECORD_UPCASTERS: &[RecordUpcaster] = elsewhere::ROWS;\n",
        )
        code, output = self.verdict(self.commit("opaque table"))
        self.assertEqual(code, 2, output)
        self.assertIn("must be an inline array", output)

    def test_a_wire_surface_bump_is_its_own_evidence(self) -> None:
        self.write("crates/demo/src/dto/hello.rs", DTO.replace("name: String", "name: Name"))
        self.write("crates/demo/src/peer.rs", PEER.replace("u32 = 1;", "u32 = 2;"))
        code, output = self.verdict(self.commit("wire bump"))
        self.assertEqual(code, 0, output)
        self.assertIn("PEER_PROTOCOL_VERSION 1 to 2 (coexist surface", output)

    def test_a_changed_wire_shape_without_the_bump_fails(self) -> None:
        self.write("crates/demo/src/dto/hello.rs", DTO.replace("name: String", "name: Name"))
        code, output = self.verdict(self.commit("wire change"))
        self.assertEqual(code, 1, output)
        self.assertIn("PEER_PROTOCOL_VERSION is 1 on both sides", output)

    def test_a_version_cannot_move_backwards(self) -> None:
        self.write("crates/demo/src/lib.rs", LIB.replace("u32 = 3;", "u32 = 2;", 1))
        code, output = self.verdict(self.commit("regress"))
        self.assertEqual(code, 1, output)
        self.assertIn("WIRE_VERSION moved backwards, 3 to 2", output)

    def test_the_default_arm_is_the_version_read(self) -> None:
        # Only the synthetic-next arm moves: the shipped version did not.
        self.write("crates/demo/src/lib.rs", WIDER_RECORD.replace("u32 = 4;", "u32 = 9;"))
        code, output = self.verdict(self.commit("synthetic arm only"))
        self.assertEqual(code, 1, output)
        self.assertIn("WIRE_VERSION is 3 on both sides", output)


class DdlCatalogEvidence(Fixture):
    """A DDL stamp's bump owes a step of its migration catalog."""

    def setUp(self) -> None:
        super().setUp()
        self.write(gate.REGISTRY, REGISTRY + DDL_SURFACE)
        self.write("crates/demo/src/store.rs", STORE)
        self.write("crates/demo/schema.sql", SCHEMA)
        self.write("crates/demo/src/migrate.rs", MIGRATIONS)
        self.base = self.commit("a DDL stamp")

    def bump(self, to: int) -> None:
        self.write("crates/demo/schema.sql", WIDER_SCHEMA)
        self.write("crates/demo/src/store.rs", STORE.replace("i32 = 7;", f"i32 = {to};"))

    def test_a_ddl_change_without_the_bump_fails(self) -> None:
        self.write("crates/demo/schema.sql", WIDER_SCHEMA)
        code, output = self.verdict(self.commit("column, no bump"))
        self.assertEqual(code, 1, output)
        self.assertIn("SCHEMA_VERSION is 7 on both sides", output)

    def test_a_ddl_bump_without_its_catalog_step_fails(self) -> None:
        self.bump(8)
        result = self.run_command(self.base, self.commit("bump, no step"))
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn("SCHEMA_VERSION moved 7 to 8 without its upgrade evidence", result.stderr)
        self.assertIn(
            "add a step to MIGRATIONS (crates/demo/src/migrate.rs) from version 7",
            result.stderr,
        )

    def test_a_ddl_bump_with_its_catalog_step_passes(self) -> None:
        self.bump(8)
        self.write(
            "crates/demo/src/migrate.rs",
            MIGRATIONS.replace('    #[cfg(feature = "synthetic-next")]\n', STEP_FROM_7),
        )
        result = self.run_command(self.base, self.commit("bump with its step"))
        self.assertEqual(result.returncode, 0, result.stdout + result.stderr)
        self.assertIn("SCHEMA_VERSION 7 to 8 (catalog steps registered)", result.stdout)

    def test_a_bump_over_two_versions_owes_the_whole_chain(self) -> None:
        self.bump(9)
        self.write(
            "crates/demo/src/migrate.rs",
            MIGRATIONS.replace('    #[cfg(feature = "synthetic-next")]\n', STEP_FROM_7),
        )
        code, output = self.verdict(self.commit("skips 8"))
        self.assertEqual(code, 1, output)
        self.assertIn("from version 8", output)

    def test_a_step_that_overshoots_the_head_is_not_its_evidence(self) -> None:
        self.bump(8)
        self.write(
            "crates/demo/src/migrate.rs",
            MIGRATIONS.replace(
                '    #[cfg(feature = "synthetic-next")]\n',
                STEP_FROM_7.replace("to_version: 8", "to_version: 9"),
            ),
        )
        code, output = self.verdict(self.commit("step to 9"))
        self.assertEqual(code, 1, output)
        self.assertIn("from version 7", output)

    def test_dropping_the_catalog_does_not_excuse_the_step(self) -> None:
        self.write("crates/demo/schema.sql", WIDER_SCHEMA)
        self.write(
            "crates/demo/src/store.rs",
            '/// version_guard(unshaped = "no longer a stamp")\n'
            "const SCHEMA_VERSION: i32 = 8;\n",
        )
        code, output = self.verdict(self.commit("drop the catalog and bump"))
        self.assertEqual(code, 1, output)
        self.assertIn("add a step to MIGRATIONS", output)

    def test_a_ddl_guard_without_a_catalog_cannot_be_evaluated(self) -> None:
        self.write(
            "crates/demo/src/store.rs",
            STORE.replace(
                '///     catalog(path = "crates/demo/src/migrate.rs", MIGRATIONS),\n', ""
            ),
        )
        code, output = self.verdict(self.commit("no catalog"))
        self.assertEqual(code, 2, output)
        self.assertIn("guards SQL DDL and declares no migration catalog", output)

    def test_a_catalog_that_cannot_be_read_is_an_error(self) -> None:
        for text, reason in (
            ("static MIGRATIONS: &[Migration] = elsewhere::ROWS;\n", "inline array"),
            ("static OTHER: &[Migration] = &[];\n", "found 0"),
            (
                'static MIGRATIONS: &[Migration] = &[Migration { id: "x" }];\n',
                "keep each row as",
            ),
        ):
            with self.subTest(reason=reason):
                self.write("crates/demo/src/migrate.rs", text)
                code, output = self.verdict(self.commit(reason))
                self.assertEqual(code, 2, output)
                self.assertIn(reason, output)

    def test_rows_selects_one_stamps_steps_of_a_shared_table(self) -> None:
        shared = """
        pub(crate) const CATALOG: &[Step] = &[
            Step { database: Database::Other, from: 7, to: 8, ddl: OTHER_DDL },
        ];
        """
        self.write("crates/demo/src/migrate.rs", shared)
        self.write(
            "crates/demo/src/store.rs",
            STORE.replace("MIGRATIONS", 'CATALOG, rows = "Database::Core"'),
        )
        base = self.commit("a shared table")
        self.write("crates/demo/schema.sql", WIDER_SCHEMA)
        self.write(
            "crates/demo/src/store.rs",
            STORE.replace("MIGRATIONS", 'CATALOG, rows = "Database::Core"').replace(
                "i32 = 7;", "i32 = 8;"
            ),
        )
        code, output = self.verdict(self.commit("another database's step"), base)
        self.assertEqual(code, 1, output)
        self.assertIn("add a step to CATALOG", output)
        self.write(
            "crates/demo/src/migrate.rs",
            shared.replace(
                "];", "    Step { database: Database::Core, from: 7, to: 8, ddl: DDL },\n];"
            ),
        )
        code, output = self.verdict(self.commit("its own step"), base)
        self.assertEqual(code, 0, output)


class Projection(Fixture):
    def test_comments_formatting_and_non_wire_derives_are_not_a_shape_change(self) -> None:
        self.write(
            "crates/demo/src/lib.rs",
            LIB.replace(
                "#[derive(Clone, Debug, Serialize, Deserialize)]",
                "/// Reworded.\n#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]",
            ).replace("pub id: String,", "pub id:   String, // the id"),
        )
        code, output = self.verdict(self.commit("cosmetic"))
        self.assertEqual(code, 0, output)

    def test_a_serde_attribute_is_a_shape_change(self) -> None:
        self.write(
            "crates/demo/src/lib.rs",
            LIB.replace("pub id: String,", '#[serde(rename = "key")]\n    pub id: String,'),
        )
        code, output = self.verdict(self.commit("rename"))
        self.assertEqual(code, 1, output)

    def test_moving_a_shape_to_another_file_is_not_a_shape_change(self) -> None:
        record = LIB[LIB.index("#[derive") : LIB.index("pub fn encode_record")]
        self.write("crates/demo/src/record.rs", record)
        self.write(
            "crates/demo/src/lib.rs",
            LIB.replace(record, "").replace(
                "items(Record, encode_record)",
                'items(encode_record), items(path = "crates/demo/src/record.rs", Record)',
            ),
        )
        code, output = self.verdict(self.commit("move Record"))
        self.assertEqual(code, 0, output)

    def test_declaring_a_marker_is_not_a_shape_change(self) -> None:
        # A base that predates its markers, and a whole-file guard over the
        # constant's own file: adding the marker must read as no change.
        marker = "/// version_guard(items(Record, encode_record))\n"
        self.write("crates/demo/src/lib.rs", LIB.replace(marker, ""))
        base = self.commit("before markers")
        self.write(
            "crates/demo/src/lib.rs",
            LIB.replace(marker, "///\n/// version_guard(\n///     file(),\n/// )\n"),
        )
        code, output = self.verdict(self.commit("declare"), base)
        self.assertEqual(code, 0, output)

    def test_a_same_named_item_is_guarded_wherever_it_repeats(self) -> None:
        twice = LIB + "\nimpl Other {\n    pub fn encode_record(&self) -> u8 {\n        1\n    }\n}\n"
        self.write("crates/demo/src/lib.rs", twice)
        base = self.commit("two encoders")
        self.write("crates/demo/src/lib.rs", twice.replace("        1\n", "        2\n"))
        code, output = self.verdict(self.commit("second encoder changes"), base)
        self.assertEqual(code, 1, output)

    def test_a_new_non_unique_index_is_elided(self) -> None:
        schema = "CREATE TABLE demo (id TEXT);\n"
        marker = "items(Record, encode_record)"
        guarded = LIB.replace(
            marker,
            marker
            + ', file(path = "crates/demo/schema.sql", cover("CREATE TABLE demo"),'
            ' elide = "sql_idempotent_index"),'
            ' catalog(path = "crates/demo/src/migrate.rs", MIGRATIONS)',
        )
        self.write("crates/demo/src/lib.rs", guarded)
        self.write("crates/demo/schema.sql", schema)
        self.write("crates/demo/src/migrate.rs", "static MIGRATIONS: &[Migration] = &[];\n")
        base = self.commit("schema")
        self.write(
            "crates/demo/schema.sql",
            schema + "CREATE INDEX IF NOT EXISTS demo_id ON demo (id);\n",
        )
        added = self.commit("add index")
        self.assertEqual(self.verdict(added, base)[0], 0)
        self.write(
            "crates/demo/schema.sql",
            schema + "CREATE INDEX IF NOT EXISTS demo_id ON demo (id, id);\n",
        )
        code, output = self.verdict(self.commit("reshape index"), added)
        self.assertEqual(code, 1, output)


class GuardSet(Fixture):
    def test_dropping_a_shape_from_the_marker_does_not_excuse_changing_it(self) -> None:
        self.write(
            "crates/demo/src/lib.rs",
            WIDER_RECORD.replace("items(Record, encode_record)", "items(encode_record)"),
        )
        code, output = self.verdict(self.commit("drop and change"))
        self.assertEqual(code, 1, output)
        self.assertIn("guarded shape changed (Record;", output)

    def test_a_guard_that_names_a_missing_item_cannot_be_evaluated(self) -> None:
        self.write(
            "crates/demo/src/lib.rs",
            LIB.replace("items(Record, encode_record)", "items(Record, decode_record)"),
        )
        code, output = self.verdict(self.commit("stale name"))
        self.assertEqual(code, 2, output)
        self.assertIn("does not find decode_record", output)

    def test_a_shapes_guard_must_cover_its_headline_shapes(self) -> None:
        self.write("crates/demo/src/dto/hello.rs", DTO.replace("Hello", "Greeting"))
        self.write("crates/demo/src/peer.rs", PEER.replace("u32 = 1;", "u32 = 2;"))
        code, output = self.verdict(self.commit("rename headline"))
        self.assertEqual(code, 2, output)
        self.assertIn("does not cover Hello", output)

    def test_a_surface_without_a_marker_cannot_be_evaluated(self) -> None:
        self.write("crates/demo/src/peer.rs", "pub const PEER_PROTOCOL_VERSION: u32 = 1;\n")
        code, output = self.verdict(self.commit("no marker"))
        self.assertEqual(code, 2, output)
        self.assertIn("declares no version_guard marker", output)

    def test_a_stated_reason_stands_in_for_a_guard(self) -> None:
        self.write(
            "crates/demo/src/peer.rs",
            '/// version_guard(unshaped = "a manual epoch")\n'
            "pub const PEER_PROTOCOL_VERSION: u32 = 1;\n",
        )
        head = self.commit("unshaped")
        code, output = self.verdict(head, head)
        self.assertEqual(code, 0, output)
        self.assertIn("(1 guarded, 1 unshaped with a stated reason)", output)

    def test_a_reason_beside_guards_is_refused(self) -> None:
        self.write(
            "crates/demo/src/peer.rs",
            '/// version_guard(unshaped = "why", items(Hello))\n'
            "pub const PEER_PROTOCOL_VERSION: u32 = 1;\n",
        )
        code, output = self.verdict(self.commit("both"))
        self.assertEqual(code, 2, output)
        self.assertIn("unshaped beside other entries", output)

    def test_a_marker_set_apart_from_its_constant_is_not_read(self) -> None:
        self.write("crates/demo/src/peer.rs", PEER.replace(")))\n", ")))\n\n"))
        code, output = self.verdict(self.commit("blank line"))
        self.assertEqual(code, 2, output)
        self.assertIn("declares no version_guard marker", output)

    def test_a_marker_may_span_lines_above_the_attributes(self) -> None:
        self.write(
            "crates/demo/src/peer.rs",
            "/// The wire.\n///\n/// version_guard(\n///     shapes(\n"
            '///         path = "crates/demo/src/dto/*.rs",\n///         cover(Hello),\n'
            "///     ),\n/// )\n#[allow(dead_code)]\npub const PEER_PROTOCOL_VERSION: u32 = 1;\n",
        )
        code, output = self.verdict(self.commit("multi-line"))
        self.assertEqual(code, 0, output)

    def test_an_unknown_marker_entry_is_refused(self) -> None:
        self.write("crates/demo/src/peer.rs", PEER.replace("shapes(", "serde("))
        code, output = self.verdict(self.commit("unknown kind"))
        self.assertEqual(code, 2, output)
        self.assertIn("unknown entry 'serde'", output)


class RegistryMoves(Fixture):
    def test_a_new_surface_is_reported_as_registered(self) -> None:
        self.write(
            gate.REGISTRY,
            REGISTRY
            + '\n[[surface]]\nconstant = "CURSOR_VERSION"\n'
            'constant_path = "crates/demo/src/cursor.rs"\nupgrade = "coexist"\n'
            'description = "fixture cursor"\n',
        )
        self.write(
            "crates/demo/src/cursor.rs",
            "/// version_guard(items(Cursor))\n"
            "pub const CURSOR_VERSION: u32 = 1;\npub struct Cursor(u64);\n",
        )
        code, output = self.verdict(self.commit("register"))
        self.assertEqual(code, 0, output)
        self.assertIn("registered by this change: crates/demo/src/cursor.rs:CURSOR_VERSION", output)
        self.assertIn("3 of 3 surfaces evaluated", output)

    def test_a_relocated_constant_keeps_its_base_version(self) -> None:
        self.write(
            gate.REGISTRY,
            REGISTRY.replace("crates/demo/src/peer.rs", "crates/demo/src/wire.rs"),
        )
        (self.repo / "crates/demo/src/peer.rs").unlink()
        self.write("crates/demo/src/wire.rs", PEER)
        self.write("crates/demo/src/dto/hello.rs", DTO.replace("name: String", "name: Name"))
        code, output = self.verdict(self.commit("relocate and change"))
        self.assertEqual(code, 1, output)
        self.assertIn("PEER_PROTOCOL_VERSION is 1 on both sides", output)

    def test_a_constant_cannot_leave_while_its_shapes_change(self) -> None:
        self.write(gate.REGISTRY, REGISTRY.split("[[surface]]\nconstant = \"PEER")[0])
        (self.repo / "crates/demo/src/peer.rs").unlink()
        removed = self.commit("retire the surface, keep the shape")
        self.assertEqual(self.verdict(removed)[0], 0)
        self.write("crates/demo/src/dto/hello.rs", DTO.replace("name: String", "name: Name"))
        code, output = self.verdict(self.git("rev-parse", "HEAD") and self.commit("and change it"))
        self.assertEqual(code, 1, output)
        self.assertIn("PEER_PROTOCOL_VERSION left the registry", output)

    def test_an_unreadable_revision_is_an_error(self) -> None:
        code, output = self.verdict("0" * 40)
        self.assertEqual(code, 2, output)

    def test_there_is_no_report_only_mode(self) -> None:
        for flag in ("--report-only", "--freeze", "--config", "--surface"):
            with self.subTest(flag=flag), contextlib.redirect_stderr(io.StringIO()):
                with self.assertRaises(SystemExit) as refused:
                    gate.main(["--base", self.base, "--head", self.base, flag, "x"])
                self.assertEqual(refused.exception.code, 2)


class PlantedView(gate.TreeView):
    """A tree with some files replaced: what committing a change to them
    would be."""

    def __init__(self, inner: gate.TreeView, files: dict[str, str]) -> None:
        super().__init__()
        self.inner = inner
        self.files = files
        self.label = f"{inner.label} with {', '.join(sorted(files))} changed"

    def matching_paths(self, patterns):
        return self.inner.matching_paths(patterns)

    def content(self, path: str) -> str | None:
        return self.files[path] if path in self.files else self.inner.content(path)

    def projected(self, path: str) -> str | None:
        return super().projected(path) if path in self.files else self.inner.projected(path)


def planted_change(view: gate.TreeView, guard: gate.Guard) -> PlantedView | None:
    """`view` with one entry `guard` projects changed, and nothing else."""
    signature = gate.guard_signature(view, guard, enforce_presence=True)
    for path, name, _ in signature:
        text = view.content(path)
        assert text is not None
        bare = name.partition("#")[0]
        if guard.kind == "file":
            candidates = [text + "\n-- a planted change\n"]
        elif guard.kind == "impls":
            trait, _, target = bare.partition(" for ")
            candidates = [
                text[: opening + 1] + " const PLANTED: () = (); " + text[opening + 1 :]
                for match in gate.RUST_SERDE_IMPL.finditer(text)
                if match.group(2) == target and trait in match.group(1)
                and (opening := text.find("{", match.end())) >= 0
            ]
        else:
            pattern = gate.RUST_DECLARATION if guard.kind == "items" else gate.RUST_SERDE_SHAPE
            candidates = [
                text[: match.start()] + "#[planted_change]\n" + text[match.start() :]
                for match in pattern.finditer(text)
                if match.group(1) == bare
            ]
        for candidate in candidates:
            changed = PlantedView(view, {path: candidate})
            after = gate.guard_signature(changed, guard, enforce_presence=False)
            if gate.comparable(after) != gate.comparable(signature):
                return changed
    return None


# What the guard-completeness audit (FIG-4566) found unguarded, by the surface
# that owes the bump: the successors of symbols the old guard inventory named
# and the tree renamed, moved or replaced, and the serialized shapes reachable
# from the newer surfaces' formats.
AUDITED_GUARDS = {
    "crates/lash-core-execution/src/tool_dispatch/context.rs:TOOL_CHILD_REBIND_VERSION": (
        "RebindSource",
    ),
    "crates/lashlang/src/artifact.rs:LASHLANG_VM_ABI_VERSION": (
        "AbilityOutcome", "ResourceOperationOutcome", "ResourceOperationBatchOutcome",
    ),
    "crates/lash-postgres-store/src/lib.rs:SCHEMA_VERSION": ("TurnCancelUndeliveredInputPolicy",),
    "crates/lash-sqlite-store/src/schema.rs:SCHEMA_VERSION": (
        "TurnCancelUndeliveredInputPolicy", "SESSION_INGRESS_TABLE", "SESSION_ROOTS_TABLES",
    ),
    "crates/lash-trace/src/lib.rs:TRACE_SCHEMA_VERSION": (
        "TraceAttemptUsageOutcome", "TraceLashlangNodeTerminalRecord", "TraceLashlangNodeReport",
    ),
    "crates/lash-core-execution/src/runtime/process/effect_summary.rs:PROCESS_EVENT_VOCABULARY_VERSION": (
        "struct ProcessEffectOccurrence",
    ),
    "crates/lash-core-execution/src/runtime/effect/tool_child.rs:TOOL_CHILD_REQUEST_VERSION": (
        "ToolAttemptLineage", "ToolCallId", "Serialize for ToolCallId",
    ),
    "crates/lash-core-store/src/session_graph.rs:SESSION_NODE_BODY_SCHEMA_VERSION": (
        "ModelConfig", "RecordedModel", "ModelKey", "ModelMetadata", "PluginConfig",
    ),
    "crates/lash-core-store/src/store/mod.rs:SESSION_HEAD_META_SCHEMA_VERSION": ("PluginConfig",),
    "crates/lashctl/src/main.rs:LASHCTL_JSON_SCHEMA_VERSION": (
        "output", "error_json", "run", "CompatRefusal", "FinalizeRefusal", "MigrationRefusal",
        "ObjectUpgradeError", "PostgresConnectionBudgetReport",
    ),
    "crates/lash-sqlite-store/src/codec.rs:SQLITE_BLOB_ENVELOPE_VERSION": (
        "StoredBlobEnvelope", "BlobCompression", "blob_envelope_admits", "encode_msgpack",
    ),
    "crates/lashlang/src/artifact.rs:MODULE_ARTIFACT_ENVELOPE_VERSION": (
        "ModuleArtifact", "ModuleRef", "HostRequirements", "ModuleExports", "Program", "Expr",
        "IrNumber", "AstString", "LashlangHostCatalog", "Serialize for ProcessType",
    ),
    "crates/lash-core-store/src/artifact_referrer.rs:ARTIFACT_REFERRER_KINDS_VERSION": (
        "HostArtifactPin", "AttachmentUploadId", "FrameEnvironmentId", "StartKey",
        "Serialize for ArtifactReferrer", "Deserialize for ArtifactReferrer",
    ),
    "crates/lash-core-store/src/store/obligation.rs:OBLIGATION_LEDGER_VOCABULARY_VERSION": (
        "ControlIntentId", "ArtifactReferrer", "canonical_id",
    ),
    "crates/lash-sansio/src/process_cursor.rs:PROCESS_CURSOR_VERSION": ("ProcessId",),
    "crates/lash-core-store/src/usage_accounting.rs:USAGE_PAYLOAD_FAMILY_VERSION": (
        "PayloadAttribution", "UsageAttemptFact", "AttemptFactOutcome",
    ),
    "crates/lash-restate/src/compat.rs:RESTATE_WIRE_VERSION": (
        "VersionRange", "RestateCompatError", "EffectGroupOpenRequest",
        "EffectGroupCommitChildResponse", "EffectGroupChildRequest", "EffectGroupNotice",
        "RestateDurableWaitAwaitRequest", "RestateProcessWorkflowInput",
        "RestateSessionDriveRequest", "UsageAccountingSettle", "ObjectUpgradeResponse",
        "BuildGeneration", "AwaitEventKey",
    ),
    "crates/lash-restate/src/session_driver.rs:LASH_TURN_OUTCOME_FORMAT_VERSION": (
        "SealVerdict", "DriveFence", "AdmissionId", "TurnOutcome", "TurnStop", "FailureCode",
    ),
    "crates/lash-restate/src/usage_accounting.rs:USAGE_ACCOUNTING_WIRE_VERSION": (
        "UsageSettlement", "UsageAttemptFact", "RunAccounting", "ModelKey", "TokenUsage",
    ),
    "crates/lash-vm-protocol/src/version.rs:WORKER_PROTOCOL_VERSION": (
        "StateDigest", "Serialize for StateDigest", "FRAME_MAGIC", "encode_parent", "WorkerLimit",
        "ProcessDefinitionId", "VersionRange",
    ),
}

# Each DDL stamp, and a row of its catalog that steps `{at}` to `{to}`.
DDL_STAMPS = {
    "crates/lash-postgres-store/src/lib.rs:SCHEMA_VERSION": (
        'ExpandMigration {{ id: "planted", from_version: {at}, to_version: {to}, statements: "" }},'
    ),
    "crates/lash-sqlite-store/src/schema.rs:SCHEMA_VERSION": (
        "SqliteMigration {{ database: SqliteDatabase::DurableCore, from: {at}, to: {to}, "
        'ddl: "" }},'
    ),
    "crates/lash-sqlite-store/src/schema.rs:PROCESS_SCHEMA_VERSION": (
        "SqliteMigration {{ database: SqliteDatabase::ProcessRegistry, from: {at}, to: {to}, "
        'ddl: "" }},'
    ),
    "crates/lash-sqlite-store/src/schema.rs:TRIGGER_SCHEMA_VERSION": (
        "SqliteMigration {{ database: SqliteDatabase::Triggers, from: {at}, to: {to}, "
        'ddl: "" }},'
    ),
}


class RealRepository(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.view = gate.WorktreeView(gate.ROOT)
        registry = cls.view.content(gate.REGISTRY)
        assert registry is not None
        cls.surfaces = {
            surface.key: surface for surface in gate.load_surfaces(registry, gate.REGISTRY)
        }

    def declaration(self, key: str) -> gate.Declaration:
        declaration = gate.declaration_in(
            self.view.content(self.surfaces[key].constant_path), self.surfaces[key]
        )
        assert declaration is not None, key
        return declaration

    def test_every_registered_surface_is_evaluated_at_head(self) -> None:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = gate.main(["--base", "HEAD", "--head", "HEAD"])
        self.assertEqual(code, 0, output.getvalue())
        self.assertRegex(output.getvalue(), r"(\d+) of \1 surfaces evaluated")
        self.assertIn("0 not evaluated", output.getvalue())

    def test_the_working_tree_against_itself_passes(self) -> None:
        result = gate.check_views(self.view, self.view)
        self.assertEqual(((), ()), (result.errors, result.failures))
        self.assertEqual(len(self.surfaces), len(result.evaluated))

    def test_a_planted_unbumped_change_to_any_guard_fails_its_surface(self) -> None:
        """Every guard of every surface: change one entry it projects, leave
        the constant alone, and the gate fails that surface."""
        planted = 0
        for key in self.surfaces:
            for guard in self.declaration(key).guards:
                with self.subTest(surface=key, guard=guard.label):
                    changed = planted_change(self.view, guard)
                    self.assertIsNotNone(changed, "no entry of the guard could be changed")
                    result = gate.check_views(self.view, changed, only=frozenset({key}))
                    self.assertEqual((), result.errors)
                    self.assertEqual([key], [finding.surface.key for finding in result.failures])
                    self.assertIn("its guarded shape changed", result.failures[0].detail)
                    planted += 1
        self.assertGreaterEqual(planted, len(self.surfaces))

    def test_the_audited_shapes_are_guarded(self) -> None:
        for key, names in AUDITED_GUARDS.items():
            with self.subTest(surface=key):
                guarded: set[str] = set()
                for guard in self.declaration(key).guards:
                    guarded.update(guard.must_cover)
                    guarded.update(
                        name.partition("#")[0]
                        for _, name, _ in gate.guard_signature(
                            self.view, guard, enforce_presence=True
                        )
                    )
                self.assertEqual([], sorted(set(names) - guarded))

    def test_the_ddl_stamps_are_the_surfaces_that_guard_ddl(self) -> None:
        stamps = {key for key in self.surfaces if self.declaration(key).catalogs}
        self.assertEqual(set(DDL_STAMPS), stamps)

    def test_a_planted_ddl_bump_owes_its_catalog_step(self) -> None:
        for key, row in DDL_STAMPS.items():
            with self.subTest(surface=key):
                surface = self.surfaces[key]
                declaration = self.declaration(key)
                (catalog,) = declaration.catalogs
                at = gate.version_at(self.view, surface)
                ddl = planted_change(self.view, declaration.guards[0])
                assert ddl is not None
                constant_file = ddl.content(surface.constant_path)
                bumped_file, bumps = re.subn(
                    rf"(const (?:BASE_)?{surface.constant}\s*:[^=;]+=\s*){at};",
                    rf"\g<1>{at + 1};",
                    constant_file,
                )
                self.assertEqual(1, bumps)
                bumped = PlantedView(ddl, {surface.constant_path: bumped_file})
                result = gate.check_views(self.view, bumped, only=frozenset({key}))
                self.assertEqual((), result.errors)
                self.assertEqual(1, len(result.failures), result)
                self.assertIn(
                    f"add a step to {catalog.label} from version {at}",
                    result.failures[0].detail,
                )

                table = bumped.content(catalog.path)
                opening = re.search(
                    rf"(?m)^[ \t]*(?:pub(?:\([^)]*\))?[ \t]+)?(?:const|static)[ \t]+"
                    rf"{catalog.table}\b",
                    table,
                )
                closing = gate.rust_item_end(table, opening.start()) - len("];")
                stepped = PlantedView(
                    bumped,
                    {
                        catalog.path: table[:closing]
                        + row.format(at=at, to=at + 1)
                        + table[closing:]
                    },
                )
                result = gate.check_views(self.view, stepped, only=frozenset({key}))
                self.assertEqual(((), ()), (result.errors, result.failures))
                self.assertEqual(
                    [f"{at} to {at + 1} (catalog steps registered)"],
                    [finding.detail for finding in result.bumped],
                )


if __name__ == "__main__":
    unittest.main()
