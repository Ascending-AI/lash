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
            ' elide = "sql_idempotent_index")',
        )
        self.write("crates/demo/src/lib.rs", guarded)
        self.write("crates/demo/schema.sql", schema)
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


class RealRepository(unittest.TestCase):
    def test_every_registered_surface_is_evaluated_at_head(self) -> None:
        output = io.StringIO()
        with contextlib.redirect_stdout(output), contextlib.redirect_stderr(output):
            code = gate.main(["--base", "HEAD", "--head", "HEAD"])
        self.assertEqual(code, 0, output.getvalue())
        self.assertRegex(output.getvalue(), r"(\d+) of \1 surfaces evaluated")
        self.assertIn("0 not evaluated", output.getvalue())


if __name__ == "__main__":
    unittest.main()
