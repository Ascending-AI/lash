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

    def test_qualified_frontends_are_refused(self):
        for entry in ["lash_typescript::parse", "lash_typescript::parse_with_globals", "lash_typescript::parse_workflow_fragment", "lash_vm::compile_program"]:
            with self.subTest(entry=entry):
                self.assertTrue(self.problems(f"fn run() {{ {entry}(source); }}"))

    def test_imported_aliases_are_refused(self):
        for source in ["use lash_typescript::{parse as parse_guest}; fn run() { parse_guest(source); }", "use lash_vm::VmInstance as GuestVm; fn run() { GuestVm::pristine(); }"]:
            with self.subTest(source=source):
                self.assertTrue(self.problems(source))

    def test_vm_and_artifact_entry_points_are_refused(self):
        for entry in ["ModuleArtifact::from_store_bytes", "LinkedModule::link", "vm.execute_compiled", "vm.compile_program"]:
            with self.subTest(entry=entry):
                self.assertTrue(self.problems(f"fn run() {{ {entry}(bytes); }}"))

    def test_worker_and_testing_items_are_allowed(self):
        self.assertFalse(self.problems("fn run() { VmInstance::pristine(); }", "lash-vm-worker"))
        self.assertFalse(self.problems('#[cfg(test)]\nmod tests { fn run() { lash_typescript::parse(source); } }'))
        self.assertFalse(self.problems("fn run() { let note = \"lash_typescript::parse(source)\"; }"))

    def test_repository_has_no_parent_vm_entry_points(self):
        self.assertEqual(CHECK.check(ROOT), [])


if __name__ == "__main__":
    unittest.main()
