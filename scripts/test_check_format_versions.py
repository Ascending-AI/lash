#!/usr/bin/env python3
"""Fixture tests for scripts/check_format_versions.py.

The fixtures build a whole miniature repository -- one source file per constant,
a manifest, and a docs tree -- and run the real check over it. That keeps the
tests honest about the vocabulary the gate actually ships: the claim patterns are
compiled from ``CONSTANTS`` at import, so a test that stubbed the table would be
exercising patterns the repository does not use.
"""

from __future__ import annotations

import importlib.util
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import unittest.mock


SCRIPT = Path(__file__).with_name("check_format_versions.py")
SPEC = importlib.util.spec_from_file_location("check_format_versions", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


# Deliberately not the values in tree: a fixture that happened to agree with the
# real repository could pass for the wrong reason.
FIXTURE_VALUES = {
    "SESSION_CHECKPOINT_SCHEMA_VERSION": "4",
    "CHECKPOINT_COMPONENT_ENCODING_VERSION": "5",
    "SESSION_HEAD_META_SCHEMA_VERSION": "6",
    "PROCESS_WAKE_DELIVERY_FORMAT_VERSION": "7",
    "SESSION_NODE_BODY_SCHEMA_VERSION": "8",
    "BYTECODE_FORMAT_VERSION": "21",
    "VM_CONTINUATION_FORMAT_VERSION": "22",
    "LASHLANG_SNAPSHOT_VERSION": "23",
    "HEAP_SIZE_SCHEDULE_VERSION": "24",
    "LASHLANG_SEGMENT_STATE_VERSION": "25",
    "RLM_SNAPSHOT_VERSION": "26",
    "LASHLANG_VM_ABI_VERSION": "lashlang-vm-abi-v27",
    "LASHLANG_SEMANTIC_HASH_VERSION": "lashlang-semantic-v28",
    "TRACE_SCHEMA_VERSION": "28",
    "REMOTE_PROTOCOL_VERSION": "29",
    "SQLITE_SCHEMA_VERSION": "30",
    "SQLITE_PROCESS_SCHEMA_VERSION": "31",
    "SQLITE_TRIGGER_SCHEMA_VERSION": "32",
    "SQLITE_EFFECT_SCHEMA_VERSION": "33",
    "POSTGRES_SCHEMA_VERSION": "34",
}

ADR = "docs/adr/0099-a-ratified-decision.md"
ADR_BODY = """# 0099. A ratified decision

## Context

The substrate moved to bytecode format 3 and RLM snapshot envelope 4.
"""


class FixtureTree:
    """A miniature repository shaped like the one the gate checks."""

    def __init__(self, values: dict[str, str] | None = None) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        self.values = dict(values or FIXTURE_VALUES)
        self.write_sources()
        self.write_manifest(
            [c.symbol for c in MODULE.CONSTANTS if c.manifest]
        )

    def write(self, relative: str, content: str) -> None:
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(content, encoding="utf-8")

    def write_sources(self) -> None:
        by_path: dict[str, list[str]] = {}
        for constant in MODULE.CONSTANTS:
            value = self.values[constant.name]
            if constant.name in {
                "LASHLANG_VM_ABI_VERSION",
                "LASHLANG_SEMANTIC_HASH_VERSION",
            }:
                line = f'pub const {constant.symbol}: &str = "{value}";'
            else:
                line = f"pub const {constant.symbol}: u32 = {value};"
            by_path.setdefault(constant.path, []).append(line)
        for path, lines in by_path.items():
            self.write(path, "\n".join(lines) + "\n")

    def write_manifest(self, symbols: list[str]) -> None:
        exports = "\n".join(f"pub use some_crate::{symbol};" for symbol in symbols)
        self.write(MODULE.MANIFEST_PATH, "//! Fixture manifest.\n\n" + exports + "\n")

    def write_page(self, body: str, relative: str = "docs/live.html") -> None:
        self.write(relative, f"<html><body><p>{body}</p></body></html>\n")

    def close(self) -> None:
        self.temporary.cleanup()


def tag(name: str, value: str) -> str:
    return f'<span data-format-version="{name}">{value}</span>'


class FormatVersionCheckTest(unittest.TestCase):
    def tree(self, values: dict[str, str] | None = None) -> FixtureTree:
        fixture = FixtureTree(values)
        self.addCleanup(fixture.close)
        return fixture

    def run_check(self, fixture: FixtureTree, historical: tuple[str, ...] = ()):
        with unittest.mock.patch.object(MODULE, "HISTORICAL_ADRS", historical):
            return MODULE.run(fixture.root)

    def details(self, report) -> str:
        return "\n".join(finding.render() for finding in report.findings)

    # -- (c) tagged claims -------------------------------------------------

    def test_a_tagged_claim_that_matches_passes(self) -> None:
        fixture = self.tree()
        fixture.write_page(
            "The RLM snapshot envelope is "
            + tag("RLM_SNAPSHOT_VERSION", "26")
            + " in this build."
        )

        report = self.run_check(fixture)

        self.assertEqual(report.findings, [], self.details(report))

    def test_the_forward_only_fence_is_checked_like_every_other_claim(self) -> None:
        # A forward-only boundary is compared for equality against the constant
        # exactly as a counter is: the fence's *value* still rots, and the
        # one-directional part is a contract the manifest states, not a reason
        # for the docs claim to go unchecked.
        fixture = self.tree()
        fixture.write_page(
            "The session node body is generation "
            + tag("SESSION_NODE_BODY_SCHEMA_VERSION", "8")
            + " in this build."
        )

        report = self.run_check(fixture)

        self.assertEqual(report.findings, [], self.details(report))

    def test_a_stale_forward_only_fence_claim_fails(self) -> None:
        fixture = self.tree()
        fixture.write_page(
            "The session node body is generation "
            + tag("SESSION_NODE_BODY_SCHEMA_VERSION", "7")
            + " in this build."
        )

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("SESSION_NODE_BODY_SCHEMA_VERSION", self.details(report))

    def test_an_untagged_node_body_claim_fails(self) -> None:
        fixture = self.tree()
        fixture.write_page("The session node body is generation 8 in this build.")

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))

    def test_a_tagged_claim_that_does_not_match_fails(self) -> None:
        fixture = self.tree()
        fixture.write_page(
            "The RLM snapshot envelope is " + tag("RLM_SNAPSHOT_VERSION", "12") + "."
        )

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        finding = report.findings[0]
        self.assertEqual(finding.path, "docs/live.html")
        self.assertIn("RLM_SNAPSHOT_VERSION is 26", finding.detail)
        self.assertIn("'12'", finding.detail)

    def test_a_tagged_identity_claim_is_compared_as_a_string(self) -> None:
        fixture = self.tree()
        fixture.write_page(
            "The VM ABI is " + tag("LASHLANG_VM_ABI_VERSION", "lashlang-vm-abi-v6") + "."
        )

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("lashlang-vm-abi-v27", report.findings[0].detail)

    def test_an_unknown_constant_name_in_a_tag_fails(self) -> None:
        fixture = self.tree()
        fixture.write_page("Something is " + tag("RLM_ENVELOPE_VERSION", "26") + ".")

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("unknown format constant 'RLM_ENVELOPE_VERSION'", report.findings[0].detail)

    # -- (d) untagged claims -----------------------------------------------

    def test_an_untagged_bare_claim_fails(self) -> None:
        fixture = self.tree()
        fixture.write_page("This release writes bytecode format version 21.")

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("untagged format version claim", report.findings[0].detail)
        self.assertEqual(report.findings[0].claim, "bytecode format version 21")

    def test_an_untagged_claim_fails_even_when_its_number_is_right(self) -> None:
        # The point is not that the number is wrong today. An untagged number is
        # unverifiable, so it will be wrong eventually and nothing will say so.
        fixture = self.tree()
        fixture.write_page("The trace schema version is 28.")

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("untagged", report.findings[0].detail)

    def test_changelog_narration_is_not_a_current_claim(self) -> None:
        # docs/remote-protocol.html is mostly this shape and stays true forever.
        fixture = self.tree()
        fixture.write_page(
            "Version 40 adds the AssistantResponseHooks runtime-effect kind, and "
            "version 39 adds the emit_trigger tool-intent kind."
        )

        report = self.run_check(fixture)

        self.assertEqual(report.findings, [], self.details(report))

    def test_a_cutover_narrative_that_pins_versions_fails(self) -> None:
        fixture = self.tree()
        fixture.write_page("The v1-to-v2 bytecode cutover has no continuation migration.")

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertEqual(report.findings[0].claim, "v1-to-v2 bytecode")

    def test_markdown_pages_are_scanned_too(self) -> None:
        fixture = self.tree()
        fixture.write("docs/architecture/notes.md", "Effect schema 11 is a cutover.\n")

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertEqual(report.findings[0].path, "docs/architecture/notes.md")

    # -- (e) historical ADRs -----------------------------------------------

    def test_an_adr_stating_a_version_fails_when_it_is_not_listed(self) -> None:
        fixture = self.tree()
        fixture.write(ADR, ADR_BODY)

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 2, self.details(report))
        self.assertEqual({f.path for f in report.findings}, {ADR})

    def test_a_listed_adr_with_the_marker_keeps_its_historical_numbers(self) -> None:
        fixture = self.tree()
        fixture.write(ADR, ADR_BODY + "\n" + MODULE.HISTORICAL_MARKER + "\n")

        report = self.run_check(fixture, historical=(ADR,))

        self.assertEqual(report.findings, [], self.details(report))

    def test_a_listed_adr_missing_the_marker_fails(self) -> None:
        fixture = self.tree()
        fixture.write(ADR, ADR_BODY)

        report = self.run_check(fixture, historical=(ADR,))

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("does not carry the marker line", report.findings[0].detail)

    def test_a_paraphrased_marker_does_not_grant_the_exemption(self) -> None:
        fixture = self.tree()
        fixture.write(
            ADR,
            ADR_BODY + "\n> Historical versions: see lash::formats for current ones.\n",
        )

        report = self.run_check(fixture, historical=(ADR,))

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("does not carry the marker line", report.findings[0].detail)

    def test_a_listed_adr_that_does_not_exist_fails(self) -> None:
        fixture = self.tree()

        report = self.run_check(fixture, historical=(ADR,))

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("does not exist", report.findings[0].detail)

    # -- (b) manifest completeness ------------------------------------------

    def test_a_manifest_constant_absent_from_formats_rs_fails(self) -> None:
        fixture = self.tree()
        fixture.write_manifest(
            [
                c.symbol
                for c in MODULE.CONSTANTS
                if c.manifest and c.symbol != "RLM_SNAPSHOT_VERSION"
            ]
        )

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("RLM_SNAPSHOT_VERSION", report.findings[0].detail)
        self.assertIn("is not re-exported", report.findings[0].detail)

    def test_a_re_export_missing_from_the_table_fails(self) -> None:
        fixture = self.tree()
        fixture.write_manifest(
            [c.symbol for c in MODULE.CONSTANTS if c.manifest] + ["NEW_FORMAT_VERSION"]
        )

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("NEW_FORMAT_VERSION", report.findings[0].detail)
        self.assertIn("is not in CONSTANTS", report.findings[0].detail)

    def test_a_non_manifest_constant_must_stay_out_of_formats_rs(self) -> None:
        # The store schema stamps and the two wire versions are excluded on
        # purpose; re-exporting one without saying so here would be silent drift.
        fixture = self.tree()
        fixture.write_manifest(
            [c.symbol for c in MODULE.CONSTANTS if c.manifest] + ["TRACE_SCHEMA_VERSION"]
        )

        report = self.run_check(fixture)

        self.assertEqual(len(report.findings), 1, self.details(report))
        self.assertIn("TRACE_SCHEMA_VERSION", report.findings[0].detail)

    # -- (a) reading the live values ----------------------------------------

    def test_a_renamed_constant_is_a_hard_error(self) -> None:
        fixture = self.tree()
        source = fixture.root / "crates/lashlang/src/runtime/state.rs"
        source.write_text(
            source.read_text(encoding="utf-8").replace(
                "LASHLANG_SNAPSHOT_VERSION", "LASHLANG_STATE_VERSION"
            ),
            encoding="utf-8",
        )

        with self.assertRaises(MODULE.CheckError) as raised:
            self.run_check(fixture)

        self.assertIn("expected exactly one definition", str(raised.exception))
        self.assertIn("found 0", str(raised.exception))

    def test_a_duplicated_constant_is_a_hard_error(self) -> None:
        fixture = self.tree()
        source = fixture.root / "crates/lash-trace/src/lib.rs"
        source.write_text(
            source.read_text(encoding="utf-8") * 2, encoding="utf-8"
        )

        with self.assertRaises(MODULE.CheckError) as raised:
            self.run_check(fixture)

        self.assertIn("found 2", str(raised.exception))

    # -- the CLI contract ----------------------------------------------------

    def cli(self, fixture: FixtureTree) -> subprocess.CompletedProcess[str]:
        return subprocess.run(
            [sys.executable, str(SCRIPT), "--repo", str(fixture.root)],
            capture_output=True,
            text=True,
        )

    def test_cli_exit_contract(self) -> None:
        # The clean case is covered in-process rather than through the CLI: a
        # fixture tree has none of the real HISTORICAL_ADRS files, which the CLI
        # cannot be told to ignore. `test_the_repository_tree_itself_passes`
        # covers the zero-finding path over a real tree.
        clean = self.tree()
        clean.write_page("Nothing versioned is claimed here.")
        self.assertEqual(self.run_check(clean).findings, [])

        failing = self.tree()
        failing.write_page("This release writes bytecode format version 3.")
        result = self.cli(failing)
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("bytecode format version 3", result.stderr)

        broken = self.tree()
        (broken.root / "crates/lash-trace/src/lib.rs").write_text("", encoding="utf-8")
        result = self.cli(broken)
        self.assertEqual(result.returncode, 2, result.stderr)
        self.assertIn("format-version check error", result.stderr)

    def test_the_repository_tree_itself_passes(self) -> None:
        report = MODULE.run(MODULE.ROOT)

        self.assertEqual(report.findings, [], self.details(report))


if __name__ == "__main__":
    unittest.main()
