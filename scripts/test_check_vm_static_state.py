#!/usr/bin/env python3
"""Fixture tests for check-vm-static-state.py, plus the whole check over the tree."""

from __future__ import annotations

import importlib.util
from pathlib import Path
import sys
import tempfile
import textwrap
import unittest


SCRIPT = Path(__file__).with_name("check-vm-static-state.py")
SPEC = importlib.util.spec_from_file_location("check_vm_static_state", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
gate = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = gate
SPEC.loader.exec_module(gate)


class CheckTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        for crate in gate.VM_CRATES:
            self.write(f"{crate}/src/lib.rs", "pub fn nothing() {}\n")
        self.allow("")

    def write(self, relative: str, text: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(textwrap.dedent(text))

    def allow(self, text: str) -> None:
        self.write(str(gate.ALLOWLIST), text)

    def problems(self) -> list[str]:
        return gate.check(self.repo)

    def test_a_new_static_is_refused(self) -> None:
        self.write(
            "crates/lashlang/src/cache.rs",
            """\
            static SEEN: Mutex<Vec<String>> = Mutex::new(Vec::new());
            """,
        )
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("crates/lashlang/src/cache.rs:1: static `SEEN` (in `<module>`)", problems[0])

    def test_every_static_form_is_refused(self) -> None:
        self.write(
            "crates/lash-typescript/src/forms.rs",
            """\
            thread_local! {
                static LOCAL: Cell<usize> = const { Cell::new(0) };
            }
            lazy_static! {
                static ref TABLE: Vec<u8> = Vec::new();
            }
            static mut COUNTER: u64 = 0;
            fn memo() -> &'static str {
                static CELL: OnceLock<String> = OnceLock::new();
                static LAZY: LazyLock<String> = LazyLock::new(String::new);
                CELL.get_or_init(String::new)
            }
            """,
        )
        names = sorted(problem.split("static `")[1].split("`")[0] for problem in self.problems())
        self.assertEqual(names, ["CELL", "COUNTER", "LAZY", "LOCAL", "TABLE"])

    def test_the_worker_crate_is_checked_once_it_exists(self) -> None:
        self.assertEqual(self.problems(), [])
        self.write(f"{gate.WORKER_CRATE}/src/main.rs", "static POOL: u8 = 0;\n")
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn(gate.WORKER_CRATE, problems[0])

    def test_an_allowlisted_static_passes_under_its_enclosing_fn(self) -> None:
        self.write(
            "crates/lashlang/src/table.rs",
            """\
            pub fn methods() -> &'static [&'static str] {
                static METHODS: LazyLock<Vec<&'static str>> = LazyLock::new(Vec::new);
                &METHODS
            }
            """,
        )
        self.allow("crates/lashlang/src/table.rs::methods::METHODS  # build table\n")
        self.assertEqual(self.problems(), [])

    def test_the_entry_names_the_scope_so_a_second_same_named_static_is_refused(self) -> None:
        self.write(
            "crates/lashlang/src/table.rs",
            """\
            fn first() {
                static ARITIES: OnceLock<u8> = OnceLock::new();
            }
            fn second() {
                static ARITIES: OnceLock<u8> = OnceLock::new();
            }
            """,
        )
        self.allow("crates/lashlang/src/table.rs::first::ARITIES  # build table\n")
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("(in `second`)", problems[0])

    def test_an_entry_without_a_reason_is_malformed(self) -> None:
        self.write("crates/lashlang/src/c.rs", "static C: u8 = 0;\n")
        self.allow("crates/lashlang/src/c.rs::<module>::C\n")
        problems = self.problems()
        self.assertTrue(any("want `<path>::<scope>::<NAME>  # <reason>`" in p for p in problems), problems)

    def test_a_stale_entry_is_refused(self) -> None:
        self.allow("crates/lashlang/src/gone.rs::<module>::GONE  # used to exist\n")
        problems = self.problems()
        self.assertEqual(len(problems), 1, problems)
        self.assertIn("matches no static item", problems[0])

    def test_lifetimes_comments_strings_and_other_trees_are_not_statics(self) -> None:
        self.write(
            "crates/lashlang/src/quiet.rs",
            """\
            // static COMMENTED: u8 = 0;
            /* static BLOCK: u8 = 0; */
            pub fn name(value: &'static str) -> &'static str {
                let _ = "static QUOTED: u8";
                let _ = '{';
                value
            }
            pub struct Holder { cell: std::sync::OnceLock<u8> }
            """,
        )
        self.write("crates/lashlang/tests/fixture.rs", "static ALLOCATOR: u8 = 0;\n")
        self.write("crates/lashlang/examples/perf.rs", "static LIVE: u8 = 0;\n")
        self.write("crates/lashlang/benches/bench.rs", "static PEAK: u8 = 0;\n")
        self.assertEqual(self.problems(), [])

    def test_the_tree_passes(self) -> None:
        self.assertEqual(gate.check(gate.ROOT), [])


if __name__ == "__main__":
    unittest.main()
