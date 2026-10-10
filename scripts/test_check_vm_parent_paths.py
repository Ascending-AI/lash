#!/usr/bin/env python3
"""Regression coverage for the production worker boundary inventory."""
import importlib.util
from pathlib import Path
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("vm_parent", ROOT / "scripts/check-vm-parent-paths.py")
CHECK = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(CHECK)


class ParentPaths(unittest.TestCase):
    def problems(self, source, crate="parent"):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            path = root / "crates" / crate / "src" / "lib.rs"
            path.parent.mkdir(parents=True)
            path.write_text(source)
            return CHECK.check(root)

    def test_qualified_dialect_entries_are_refused(self):
        for entry in ["lash_dialect_typescript::lower", "lash_dialect_typescript::lower_kernel_text", "lash_dialect_python::lower", "lash_dialect_typescript::parse"]:
            with self.subTest(entry=entry):
                self.assertTrue(self.problems(f"fn run() {{ {entry}(source, &environment); }}"))

    def test_imported_aliases_are_refused(self):
        for source in ["use lash_dialect_typescript::{lower as lower_guest}; fn run() { lower_guest(source); }", "use lash_kernel_vm::KernelMachine as GuestVm; fn run() { GuestVm::import(program, bounds, parked); }"]:
            with self.subTest(source=source):
                self.assertTrue(self.problems(source))

    def test_machine_entry_points_are_refused(self):
        for entry in ["KernelMachine::start", "KernelMachine::import", "Machine::start"]:
            with self.subTest(entry=entry):
                self.assertTrue(self.problems(f"fn run() {{ {entry}(program, bounds, start); }}"))

    def test_printing_a_document_is_allowed(self):
        self.assertFalse(self.problems("fn show() { lash_dialect_typescript::print(&document); }"))

    def test_worker_kernel_and_testing_items_are_allowed(self):
        self.assertFalse(self.problems("fn run() { KernelMachine::start(program, bounds, start); }", "lash-vm-worker"))
        self.assertFalse(self.problems("fn run() { KernelMachine::start(program, bounds, start); }", "lash-kernel-vm"))
        self.assertFalse(self.problems('#[cfg(test)]\nmod tests { fn run() { lash_dialect_typescript::lower(source, &environment); } }'))
        self.assertFalse(self.problems("fn run() { let note = \"lash_dialect_typescript::lower(source)\"; }"))

    def test_repository_has_no_parent_vm_entry_points(self):
        self.assertEqual(CHECK.check(ROOT), [])


if __name__ == "__main__":
    unittest.main()
