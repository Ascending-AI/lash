#!/usr/bin/env python3

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
SCRIPT = ROOT / "scripts/check-substrate-boundary.sh"
ALLOWLIST = ROOT / "scripts/drive-determinism-allowlist.txt"
COUNT = ROOT / "scripts/drive-determinism-allowlist.count"

ENTRY_SEPARATOR = "  |  "


def parse_allowlist(text: str) -> list[tuple[str, str, int]]:
    entries = []
    for line in text.splitlines():
        if not line.strip() or line.startswith("#"):
            continue
        body = line.split("  # ", 1)[0]
        path, text, count = body.split(ENTRY_SEPARATOR)
        entries.append((path, text, int(count)))
    return entries


# Every path the script hands to rg must exist in a fixture tree, or the
# search exits nonzero and the check reports a search failure instead of a
# rule verdict. Glob entries need a file that satisfies them.
FIXTURE_DIRS = [
    "crates/lash-core/src/runtime/turn_loop",
    "crates/lash-core/src/runtime/turn_driver",
    "crates/lash-core-ids/src",
    "crates/lash-core-llm/src",
    "crates/lash/src",
    "crates/lash-core-execution/src/session",
    "crates/lash-core-execution/src/tool_dispatch",
    "crates/lash-core-execution/src/runtime/effect",
    "crates/lash-protocol-rlm/src/executor",
    "crates/lash-protocol-rlm/src/projection",
    "crates/lashlang/src",
    "crates/lash-lashlang-runtime/src",
    "crates/lash-restate/src/controller",
    "crates/lash-restate/src/effect_group",
    "crates/lash-restate/src/process",
]
FIXTURE_FILES = [
    "crates/lash-core/src/runtime/logical_turn.rs",
    "crates/lash-core/src/runtime/turn_boundary.rs",
    "crates/lash-core-execution/src/session.rs",
    "crates/lash-core-execution/src/tool_dispatch.rs",
    "crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs",
    "crates/lash-core-execution/src/runtime/effect/group.rs",
    "crates/lash-restate/src/effect_group.rs",
    "crates/lash-restate/src/durable_wait.rs",
]
FIXTURE_DRIVE_FILE = "crates/lash-core/src/runtime/logical_turn.rs"
FIXTURE_HIT_LINE = "    tokio::spawn(worker());"
FIXTURE_ENGINE_ID_FILE = "crates/lash-core/src/runtime/turn_loop/engine_ids.rs"
FIXTURE_ENGINE_ID_LINE = "    let _ = context.restate_invocation_id();"


class DriveDeterminismRatchetTests(unittest.TestCase):
    def run_check(self, cwd: Path) -> subprocess.CompletedProcess:
        return subprocess.run(
            ["bash", str(cwd / "scripts" / SCRIPT.name)],
            cwd=cwd,
            text=True,
            capture_output=True,
            check=False,
        )

    def build_fixture(
        self, root: Path, drive_lines: list[str], allowlist_entries: list[str]
    ) -> None:
        scripts = root / "scripts"
        scripts.mkdir(parents=True)
        shutil.copy2(SCRIPT, scripts / SCRIPT.name)
        (scripts / ALLOWLIST.name).write_text("\n".join(allowlist_entries) + "\n")
        for directory in FIXTURE_DIRS:
            (root / directory).mkdir(parents=True)
        for file in FIXTURE_FILES:
            path = root / file
            path.parent.mkdir(parents=True, exist_ok=True)
            path.touch()
        (root / FIXTURE_DRIVE_FILE).write_text("\n".join(drive_lines) + "\n")

    def test_drive_determinism_rule_passes_on_the_tree(self) -> None:
        result = subprocess.run(
            ["bash", str(SCRIPT)],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=False,
        )
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_allowlist_only_shrinks(self) -> None:
        # Every entry pins a forbidden-construct site as
        # `path  |  <normalized line text>  |  <occurrence count>  # id`.
        # A fix deletes or decrements entries; nothing may grow them, so the
        # total pinned occurrence count is capped by the committed count file.
        entries = parse_allowlist(ALLOWLIST.read_text())
        for path, text, count in entries:
            with self.subTest(path=path, text=text):
                self.assertTrue(path.startswith("crates/"), path)
                self.assertTrue(text, path)
                self.assertGreaterEqual(count, 1, path)
        cap = int(COUNT.read_text().strip())
        self.assertLessEqual(sum(count for _, _, count in entries), cap)

    def test_blank_lines_above_a_hit_still_pass(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(
                root,
                ["fn drive() {", FIXTURE_HIT_LINE, "}"],
                [
                    f"{FIXTURE_DRIVE_FILE}{ENTRY_SEPARATOR}"
                    f"tokio::spawn(worker());{ENTRY_SEPARATOR}1  # UNMAPPED"
                ],
            )
            drive_file = root / FIXTURE_DRIVE_FILE
            drive_file.write_text("\n\n\n" + drive_file.read_text())
            result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_new_hit_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(
                root,
                ["fn drive() {", FIXTURE_HIT_LINE, "    let _id = Uuid::new_v4();", "}"],
                [
                    f"{FIXTURE_DRIVE_FILE}{ENTRY_SEPARATOR}"
                    f"tokio::spawn(worker());{ENTRY_SEPARATOR}1  # UNMAPPED"
                ],
            )
            result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rule 5 failed", result.stderr)

    def test_extra_occurrence_of_a_pinned_hit_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(
                root,
                ["fn drive() {", FIXTURE_HIT_LINE, FIXTURE_HIT_LINE, "}"],
                [
                    f"{FIXTURE_DRIVE_FILE}{ENTRY_SEPARATOR}"
                    f"tokio::spawn(worker());{ENTRY_SEPARATOR}1  # UNMAPPED"
                ],
            )
            result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rule 5 failed", result.stderr)

    def test_engine_named_identifier_in_a_kernel_crate_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(root, ["fn drive() {", "}"], [])
            (root / FIXTURE_ENGINE_ID_FILE).write_text(FIXTURE_ENGINE_ID_LINE + "\n")
            result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("rule 4 failed", result.stderr)

    def test_engine_named_identifier_in_the_engine_crate_passes(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(root, ["fn drive() {", "}"], [])
            hit = root / "crates/lash-restate/src/controller/engine_ids.rs"
            hit.write_text(FIXTURE_ENGINE_ID_LINE + "\n")
            result = self.run_check(root)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_removed_hit_with_stale_entry_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            self.build_fixture(
                root,
                ["fn drive() {", "}"],
                [
                    f"{FIXTURE_DRIVE_FILE}{ENTRY_SEPARATOR}"
                    f"tokio::spawn(worker());{ENTRY_SEPARATOR}1  # UNMAPPED"
                ],
            )
            result = self.run_check(root)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("stale allowlist entry", result.stderr)


if __name__ == "__main__":
    unittest.main()
