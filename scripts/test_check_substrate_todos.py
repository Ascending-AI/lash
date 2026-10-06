#!/usr/bin/env python3
"""Self-tests for scripts/check-substrate-todos.py."""

from __future__ import annotations

import subprocess
import sys
import tempfile
import unittest
from pathlib import Path

SCRIPT = Path(__file__).resolve().parent / "check-substrate-todos.py"
STUB_FILE = "crates/lash-core-execution/src/runtime/actor/turn.rs"


class SubstrateTodosTests(unittest.TestCase):
    def run_check(self, files: dict[str, str], *flags: str) -> subprocess.CompletedProcess:
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            for relative, text in files.items():
                path = root / relative
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text(text)
            return subprocess.run(
                [sys.executable, str(SCRIPT), "--root", str(root), *flags],
                text=True,
                capture_output=True,
                check=False,
            )

    def test_tagged_stubs_pass_and_are_counted_per_lane(self) -> None:
        result = self.run_check({STUB_FILE: (
            'fn a() { todo!("V0 (FIG-5170): admit the turn") }\n'
            "fn b() {\n"
            "    todo!(\n"
            '        "L4 (FIG-5174): run the round"\n'
            "    )\n"
            "}\n"
            'fn c() { unimplemented!("L4 (FIG-5174): settle") }\n'
        )})
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("V0 1, L4 2 (3 in all)", result.stdout)

    def test_an_untagged_stub_fails(self) -> None:
        for body in ("todo!()", 'todo!("later")', "unimplemented!()"):
            with self.subTest(body=body):
                result = self.run_check({STUB_FILE: f"fn a() {{ {body} }}\n"})
                self.assertNotEqual(result.returncode, 0)
                self.assertIn(f"{STUB_FILE}:1: a stub without a known lane tag", result.stderr)

    def test_a_tag_naming_another_lanes_ticket_or_an_unknown_lane_fails(self) -> None:
        for tag in ("V0 (FIG-5174): admit", "L99 (FIG-1): admit"):
            with self.subTest(tag=tag):
                result = self.run_check({STUB_FILE: f'fn a() {{ todo!("{tag}") }}\n'})
                self.assertNotEqual(result.returncode, 0)

    def test_comments_strings_and_witnesses_are_not_stubs(self) -> None:
        result = self.run_check({
            STUB_FILE: (
                "/// ```compile_fail\n"
                "/// fn f() { unimplemented!() }\n"
                "/// ```\n"
                "fn a() -> &'static str { \"todo!()\" } // todo!()\n"
            ),
            "crates/lash/tests/processes_evidence.rs": "fn w() { let _ = f(todo!()); }\n",
            "crates/lash-render/src/lib.rs": "fn outside() { todo!() }\n",
        })
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn("none (0 in all)", result.stdout)

    def test_final_fails_on_any_remaining_stub(self) -> None:
        tagged = {STUB_FILE: 'fn a() { todo!("L5 (FIG-5173): resolve") }\n'}
        result = self.run_check(tagged, "--final")
        self.assertNotEqual(result.returncode, 0)
        self.assertIn(f"{STUB_FILE}:1: L5 stub remains", result.stderr)
        self.assertEqual(self.run_check({STUB_FILE: "fn a() {}\n"}, "--final").returncode, 0)


if __name__ == "__main__":
    unittest.main()
