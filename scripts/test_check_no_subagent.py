#!/usr/bin/env python3
"""Self-tests for scripts/check-no-subagent.py."""

from __future__ import annotations

import importlib.util
import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "check-no-subagent.py"
SPEC = importlib.util.spec_from_file_location("check_no_subagent", SCRIPT)
gate = importlib.util.module_from_spec(SPEC)
assert SPEC.loader is not None
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)

WORD = "sub" + "agent"


class NoSubagentTests(unittest.TestCase):
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

    def findings(self, files: dict[str, str]) -> list[str]:
        with tempfile.TemporaryDirectory() as tmp:
            return gate.check(self.repo(tmp, files))

    def test_the_gate_passes_on_the_tree(self) -> None:
        result = subprocess.run([sys.executable, str(SCRIPT)], text=True,
                                capture_output=True, check=False)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_planted_mention_fails(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = self.repo(tmp, {"crates/lash-core/src/lib.rs": f"// spawns a {WORD}\n"})
            result = subprocess.run([sys.executable, str(SCRIPT), "--root", str(root)],
                                    text=True, capture_output=True, check=False)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("crates/lash-core/src/lib.rs:1: a subagent concept in core", result.stderr)

    def test_every_guarded_crate_and_spelling_fails(self) -> None:
        crates = ("lash-core", "lash-core-execution", "lash-core-store", "lash-sansio",
                  "lash-durable", "lash")
        spellings = (WORD, WORD.upper(), f"{WORD[:3].title()}{WORD[3:].title()}Context",
                     f"MAX_{WORD.upper()}_DEPTH", f"{WORD}s")
        for crate in crates:
            for text in spellings:
                with self.subTest(crate=crate, text=text):
                    found = self.findings({f"crates/{crate}/src/a.rs": f"x\n{text}\n"})
                    self.assertEqual(len(found), 1, found)

    def test_a_mention_in_a_guarded_path_fails(self) -> None:
        found = self.findings({f"crates/lash/tests/{WORD}_laws.rs": "nothing here\n"})
        self.assertEqual(len(found), 1, found)
        self.assertTrue(found[0].startswith(f"crates/lash/tests/{WORD}_laws.rs: "), found)

    def test_examples_docs_and_other_crates_pass(self) -> None:
        self.assertEqual(self.findings({
            f"examples/delegation/src/{WORD}.rs": f"// a {WORD}\n",
            "docs/adr/0134.md": f"lash ships no {WORD} implementation\n",
            "crates/lash-durable-test/tests/a.rs": f"// {WORD}\n",
            "crates/lash-sim/src/a.rs": f"// {WORD}\n",
            "crates/lash-vm/src/a.rs": f"// {WORD}\n",
        }), [])

    def test_an_untracked_file_is_not_read(self) -> None:
        with tempfile.TemporaryDirectory() as tmp:
            root = self.repo(tmp, {"crates/lash/src/lib.rs": "// lash\n"})
            (root / "crates/lash/src/notes.rs").write_text(f"// {WORD}\n")
            self.assertEqual(gate.check(root), [])


if __name__ == "__main__":
    unittest.main()
