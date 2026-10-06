#!/usr/bin/env python3
"""Fixture tests for check-substrate-port-ledger.py."""

from __future__ import annotations

import json
import shutil
import subprocess
import tempfile
import unittest
from pathlib import Path

ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check-substrate-port-ledger.py"
LEDGER = "docs/testing/substrate-port-ledger.toml"
DOUBLE = "lash_restate" + "_test"
PACKAGE = "lash-restate" + "-test"

PORTED = "crates/app/tests/port.rs"
MANIFEST = "crates/app/Cargo.toml"
RESTATE_TEST = "crates/lash-restate/src/tests/law.rs"

FILES = {
    "Cargo.toml": f'[workspace.dependencies]\n{PACKAGE} = {{ path = "crates/{PACKAGE}" }}\n',
    f"crates/{PACKAGE}/Cargo.toml": f'[package]\nname = "{PACKAGE}"\n',
    f"crates/{PACKAGE}/src/lib.rs": "pub fn backend() {}\n",
    "crates/lash-restate/src/lib.rs": "pub fn engine() {}\n",
    RESTATE_TEST: "#[test]\nfn a_restate_law() {}\n",
    MANIFEST: f"[dev-dependencies]\n{PACKAGE} = {{ workspace = true }}\n",
    PORTED: (
        f"use {DOUBLE}::backend;\n"
        "#[tokio::test]\nasync fn a_port_law() {}\n"
        "#[tokio::test]\nasync fn a_journal_law() {}\n"
    ),
    "crates/app/src/lib.rs": "pub fn app() {}\n",
}


def row(path: str, **fields) -> dict:
    base = {
        "path": path, "test": "*", "area": "turns", "needs": ["turn"], "class": "mechanical",
        "lane": "L9b", "laws": ["a_port_law"], "disposition": "port", "status": "todo",
    }
    return {**base, **fields}


ROWS = [
    row(PORTED),
    row(PORTED, test="a_journal_law", **{"class": "delete"}, laws=["a_journal_law"],
        disposition="subject-deleted"),
    row(MANIFEST, area="manifest", needs=[], **{"class": "delete"}, lane="L10a",
        laws=["the dependency"], disposition="subject-deleted"),
    row(RESTATE_TEST, area="waits", **{"class": "delete"}, lane="L10a", laws=["a_restate_law"],
        disposition="covered-by:planned:F1"),
]
PLANNED = {"F1": {"owner": "L2", "law": "a zombie's commit is refused"}}


def render(rows: list[dict], planned: dict) -> str:
    lines = ["# ledger", ""]
    for name, law in planned.items():
        lines.append(f"[planned.{json.dumps(name)}]")
        lines.extend(f"{key} = {json.dumps(value)}" for key, value in law.items())
        lines.append("")
    for entry in rows:
        lines.append("[[row]]")
        lines.extend(f"{key} = {json.dumps(value)}" for key, value in entry.items())
        lines.append("")
    return "\n".join(lines)


class SubstratePortLedgerTests(unittest.TestCase):
    def tree(
        self,
        files: dict[str, str | None] | None = None,
        rows: list[dict] | None = None,
        planned: dict | None = None,
    ) -> Path:
        root = Path(tempfile.mkdtemp())
        self.addCleanup(shutil.rmtree, root)
        (root / "scripts").mkdir()
        shutil.copy(SCRIPT, root / "scripts" / SCRIPT.name)
        for path, text in {**FILES, **(files or {})}.items():
            if text is None:
                continue
            file = root / path
            file.parent.mkdir(parents=True, exist_ok=True)
            file.write_text(text)
        ledger = root / LEDGER
        ledger.parent.mkdir(parents=True)
        ledger.write_text(render(ROWS if rows is None else rows, PLANNED if planned is None else planned))
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        self.check(root, "--write-summary")
        return root

    def check(self, root: Path, *args: str) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["python3", str(root / "scripts" / SCRIPT.name), *args],
            cwd=root, text=True, capture_output=True,
        )

    def assert_passes(self, root: Path, *args: str) -> None:
        result = self.check(root, *args)
        self.assertEqual(result.returncode, 0, result.stderr)

    def assert_fails(self, root: Path, needle: str, *args: str) -> None:
        result = self.check(root, *args)
        self.assertEqual(result.returncode, 1, result.stdout + result.stderr)
        self.assertIn(needle, result.stderr)

    def test_a_complete_ledger_passes(self) -> None:
        self.assert_passes(self.tree())

    def test_a_planted_file_that_uses_the_double_without_a_row_fails(self) -> None:
        root = self.tree({"crates/app/tests/planted.rs": f"use {DOUBLE}::backend;\n"})
        self.assert_fails(root, "crates/app/tests/planted.rs: uses the Restate server double but has no ledger row")

    def test_a_planted_manifest_that_depends_on_the_double_without_a_row_fails(self) -> None:
        root = self.tree({"crates/other/Cargo.toml": f"[dev-dependencies]\n{PACKAGE} = {{ workspace = true }}\n"})
        self.assert_fails(root, "crates/other/Cargo.toml: uses the Restate server double")

    def test_a_planted_restate_crate_test_without_a_row_fails(self) -> None:
        root = self.tree({
            f"crates/{PACKAGE}/tests/planted.rs": "fn helper() {}\n",
            "crates/lash-restate/src/wire.rs": "#[test]\nfn a_wire_law() {}\n",
        })
        self.assert_fails(root, f"crates/{PACKAGE}/tests/planted.rs: uses the Restate server double")
        self.assert_fails(root, "crates/lash-restate/src/wire.rs: uses the Restate server double")

    def test_a_planted_deletion_without_a_disposition_fails(self) -> None:
        for disposition in ("", "port", "owed", "covered-by:"):
            with self.subTest(disposition=disposition):
                rows = [*ROWS[:3], {**ROWS[3], "status": "deleted", "disposition": disposition}]
                self.assert_fails(self.tree(rows=rows), "a deleted row needs a disposition")

    def test_a_deletion_with_a_disposition_passes(self) -> None:
        for disposition in ("subject-deleted", "covered-by:planned:F1", "replaced-by:crates/app/tests/new.rs::a_law"):
            with self.subTest(disposition=disposition):
                rows = [*ROWS[:3], {**ROWS[3], "status": "deleted", "disposition": disposition}]
                self.assert_passes(self.tree(rows=rows))

    def test_a_todo_row_whose_file_is_gone_fails_while_the_double_exists(self) -> None:
        root = self.tree({PORTED: None, "crates/app/tests/renamed.rs": f"use {DOUBLE}::backend;\n"})
        self.assert_fails(root, f"{PORTED} no longer exists but its row is still todo")

    def test_a_todo_row_whose_file_is_gone_passes_once_the_double_is_deleted(self) -> None:
        root = self.tree({
            PORTED: None, MANIFEST: "[dev-dependencies]\n", RESTATE_TEST: None,
            "crates/lash-restate/src/lib.rs": None,
            f"crates/{PACKAGE}/Cargo.toml": None, f"crates/{PACKAGE}/src/lib.rs": None,
        })
        self.assert_passes(root)

    def test_a_todo_function_row_its_file_no_longer_defines_fails(self) -> None:
        root = self.tree({PORTED: f"use {DOUBLE}::backend;\n#[tokio::test]\nasync fn a_port_law() {{}}\n"})
        self.assert_fails(root, f"{PORTED} no longer defines a_journal_law")

    def test_a_function_row_without_its_file_row_fails(self) -> None:
        self.assert_fails(self.tree(rows=ROWS[1:]), f"{PORTED}::a_journal_law has no '*' row")

    def test_malformed_rows_fail(self) -> None:
        cases = {
            "class must be one of": {"class": "port"},
            "lane must be one of": {"lane": "L9z"},
            "needs must be a list": {"needs": ["turn", "journal"]},
            "laws must name at least one law": {"laws": []},
            "status must be one of": {"status": "done"},
            "only a mechanical or semantic row is ported": {"class": "delete", "status": "ported"},
            "disposition is 'port' until it is ported": {"disposition": "subject-deleted"},
        }
        for needle, change in cases.items():
            with self.subTest(needle=needle):
                self.assert_fails(self.tree(rows=[{**ROWS[0], **change}, *ROWS[1:]]), needle)

    def test_a_replace_row_must_name_the_law_it_owes(self) -> None:
        replace = {**ROWS[3], "class": "replace", "lane": "L9c", "disposition": "owed"}
        self.assert_fails(self.tree(rows=[*ROWS[:3], replace]), "names the law it owes")
        self.assert_passes(self.tree(rows=[*ROWS[:3], {**replace, "owed": "a wait's first resolution wins"}]))

    def test_a_reference_to_an_unknown_law_fails(self) -> None:
        for disposition, needle in (
            ("covered-by:planned:F9", "which [planned] does not define"),
            ("covered-by:row:crates/app/tests/missing.rs::a_law", "which the ledger does not have"),
        ):
            with self.subTest(disposition=disposition):
                rows = [*ROWS[:3], {**ROWS[3], "disposition": disposition}]
                self.assert_fails(self.tree(rows=rows), needle)
        rows = [*ROWS[:3], {**ROWS[3], "disposition": f"covered-by:row:{PORTED}::a_port_law"}]
        self.assert_passes(self.tree(rows=rows))

    def test_a_stale_summary_fails_until_it_is_rewritten(self) -> None:
        root = self.tree()
        ledger = root / LEDGER
        ledger.write_text(ledger.read_text().replace('lane = "L9b"', 'lane = "L9c"', 1))
        self.assert_fails(root, "the coverage summary is stale")
        self.assert_passes(root, "--write-summary")

    def test_the_summary_lists_the_laws_owed(self) -> None:
        replace = {**ROWS[3], "class": "replace", "lane": "L9c", "disposition": "owed",
                   "owed": "a wait's first resolution wins"}
        root = self.tree(rows=[*ROWS[:3], replace])
        text = (root / LEDGER).read_text()
        self.assertIn(f"# - L9c: {RESTATE_TEST}\n#   a wait's first resolution wins", text)
        self.assertIn("By class: mechanical 1, semantic 0, delete 2, replace 1.", text)

    def test_final_mode_fails_on_todo_rows_and_remaining_references(self) -> None:
        root = self.tree()
        self.assert_fails(root, f"{PORTED}::* is still todo (L9b)", "--final")
        self.assert_fails(root, f"{PORTED}:1: the Restate server double is still referenced", "--final")
        self.assert_fails(root, "planned law F1 (L2) has not landed", "--final")

    def test_final_mode_passes_once_every_row_is_finished(self) -> None:
        rows = [
            {**ROWS[0], "status": "ported"},
            {**ROWS[1], "status": "deleted"},
            {**ROWS[2], "status": "deleted"},
            {**ROWS[3], "status": "deleted"},
        ]
        planned = {"F1": {**PLANNED["F1"], "path": "crates/app/tests/fence.rs::a_zombie_commit_is_refused"}}
        root = self.tree({
            PORTED: "#[tokio::test]\nasync fn a_port_law() {}\n",
            MANIFEST: "[dev-dependencies]\n",
            "Cargo.toml": "[workspace]\n",
            "crates/app/tests/fence.rs": "#[tokio::test]\nasync fn a_zombie_commit_is_refused() {}\n",
            RESTATE_TEST: None, "crates/lash-restate/src/lib.rs": None,
            f"crates/{PACKAGE}/Cargo.toml": None, f"crates/{PACKAGE}/src/lib.rs": None,
        }, rows=rows, planned=planned)
        self.assert_passes(root, "--final")

    def test_final_mode_fails_on_a_missing_covering_law(self) -> None:
        rows = [
            {**ROWS[0], "status": "ported"},
            {**ROWS[1], "status": "deleted", "disposition": "covered-by:crates/app/tests/gone.rs::a_law"},
            {**ROWS[2], "status": "deleted"},
            {**ROWS[3], "status": "deleted", "disposition": "subject-deleted"},
        ]
        root = self.tree({
            PORTED: "#[tokio::test]\nasync fn a_port_law() {}\n",
            MANIFEST: "[dev-dependencies]\n", "Cargo.toml": "[workspace]\n",
            RESTATE_TEST: None, "crates/lash-restate/src/lib.rs": None,
            f"crates/{PACKAGE}/Cargo.toml": None, f"crates/{PACKAGE}/src/lib.rs": None,
        }, rows=rows, planned={})
        self.assert_fails(root, "names crates/app/tests/gone.rs::a_law, which does not exist", "--final")


if __name__ == "__main__":
    unittest.main()
