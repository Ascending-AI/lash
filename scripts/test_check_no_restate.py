#!/usr/bin/env python3
"""Self-tests for scripts/check-no-restate.py."""

from __future__ import annotations

import importlib.util
import re
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "check-no-restate.py"
SPEC = importlib.util.spec_from_file_location("check_no_restate", SCRIPT)
gate = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)

ENGINE = "Re" + "state"


class NoRestateTests(unittest.TestCase):
    def repo(self, tmp: str, files: dict[str, str]) -> Path:
        """A git repository tracking `files`."""
        root = Path(tmp)
        for relative, text in files.items():
            path = root / relative
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(text)
        subprocess.run(["git", "init", "-q"], cwd=root, check=True)
        subprocess.run(["git", "add", "-A"], cwd=root, check=True)
        return root

    def findings(self, files: dict[str, str], pending=()) -> list[str]:
        with tempfile.TemporaryDirectory() as tmp:
            return gate.check(self.repo(tmp, files), pending)

    def test_the_gate_passes_on_the_tree(self) -> None:
        result = subprocess.run([sys.executable, str(SCRIPT)], text=True,
                                capture_output=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_planted_mention_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = self.repo(tmp, {"crates/a/src/lib.rs": f"// runs on {ENGINE}\n"})
            result = subprocess.run([sys.executable, str(SCRIPT), "--root", str(root)],
                                    text=True, capture_output=True, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("crates/a/src/lib.rs:1: a Restate mention", result.stderr)

    def test_every_spelling_fails(self) -> None:
        for text in (ENGINE, ENGINE.upper(), ENGINE.lower(), f"lash_{ENGINE.lower()}::x",
                     f"{ENGINE.lower()}d by hand", f"a {ENGINE.lower()}ment"):
            with self.subTest(text=text):
                self.assertEqual(len(self.findings({"docs/a.md": f"x\n{text}\n"})), 1)

    def test_a_mention_in_a_tracked_path_fails(self) -> None:
        found = self.findings({f"docs/{ENGINE.lower()}-notes.md": "nothing here\n"})
        self.assertEqual(found, [f"docs/{ENGINE.lower()}-notes.md: a Restate mention: "
                                 f"docs/{ENGINE.lower()}-notes.md"])

    def test_a_camel_case_join_passes(self) -> None:
        self.assertEqual(self.findings({"crates/a/src/lib.rs": "trait FixtureState {}\n"
                                                               "struct StoreState;\n"}), [])

    def test_the_gates_own_name_passes_but_not_a_mention_beside_it(self) -> None:
        wiring = "      - id: no-restate\n        entry: python3 scripts/check-no-restate.py\n"
        self.assertEqual(self.findings({".pre-commit-config.yaml": wiring}), [])
        found = self.findings({"scripts/push-gate.sh": f"  python3 scripts/check-no-restate.py  # {ENGINE}\n"})
        self.assertEqual(len(found), 1)

    def test_an_untracked_file_is_not_read(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = self.repo(tmp, {"README.md": "lash\n"})
            (root / "notes.md").write_text(f"{ENGINE}\n")
            self.assertEqual(gate.check(root, ()), [])

    def test_a_permanent_exception_passes_and_a_line_scoped_one_holds_only_its_line(self) -> None:
        prompt = "crates/lash-protocol-standard/src/prompt.rs"
        self.assertEqual(self.findings({
            "docs/testing/substrate-port-ledger.toml": f"# {ENGINE} double\n",
            prompt: '"- do not restate conclusions."\n',
        }), [])
        found = self.findings({prompt: '"- do not restate conclusions."\n'
                                       f"// the {ENGINE} host\n"})
        self.assertEqual(len(found), 1)
        self.assertTrue(found[0].startswith(f"{prompt}:2:"), found)

    def test_a_settled_adr_and_its_index_row_pass_and_a_live_adr_fails(self) -> None:
        settled = f"docs/adr/0104-{ENGINE.lower()}-engine.md"
        files = {
            settled: f"# 0104: {ENGINE} engine\n\n## Status\n\nReplaced by ADR 0132.\n",
            "docs/adr/0105-retired.md": f"# 0105: x\n\n## Status\n\nRetired: {ENGINE} names.\n",
            "docs/adr/0132-live.md": f"# 0132: y\n\n## Status\n\nAccepted.\n\n{ENGINE} is gone.\n",
            "docs/adr/README.md": (f"| 0104 | [{ENGINE} engine](0104-{ENGINE.lower()}-engine.md) |\n"
                                   f"| 0132 | [{ENGINE} y](0132-live.md) |\n"),
        }
        found = self.findings(files)
        self.assertEqual(sorted(line.split(": ")[0] for line in found),
                         ["docs/adr/0132-live.md:7", "docs/adr/README.md:2"])

    def test_a_pending_exception_passes_while_it_matches_and_fails_once_stale(self) -> None:
        pending = (gate.Allowance(("scripts/e2e.py", "docs/e2e.md"), "L9h removes it"),)
        files = {"scripts/e2e.py": f"SERVER = '{ENGINE.lower()}'\n", "docs/e2e.md": f"{ENGINE}\n"}
        self.assertEqual(self.findings(files, pending), [])
        files["docs/e2e.md"] = "the durable engine\n"
        found = self.findings(files, pending)
        self.assertEqual(len(found), 1)
        self.assertRegex(found[0], re.escape("docs/e2e.md: stale pending exception"))

    def test_every_pending_exception_names_its_owner(self) -> None:
        for allowance in gate.PENDING:
            with self.subTest(paths=allowance.paths):
                self.assertTrue(allowance.reason.strip())


if __name__ == "__main__":
    unittest.main()
