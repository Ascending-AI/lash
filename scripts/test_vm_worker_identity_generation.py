#!/usr/bin/env python3
"""The Buck2 graph derives the VM worker fingerprint without source mutation."""

import json
import pathlib
import sys
import unittest

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/buck2"))
import generate_model


class WorkerIdentityGenerationTests(unittest.TestCase):
    def test_inventory_does_not_generate_worker_identity(self):
        inventory = json.loads(
            (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
        )
        labels = {
            target["label"]
            for package in inventory["packages"]
            for target in package["targets"]
            if target.get("label")
        }
        self.assertFalse(any("identity.rs" in label for label in labels))
        self.assertFalse((ROOT / "crates/lash-vm-worker/src/identity.rs").exists())

    def test_both_vm_build_scripts_hash_the_declared_worker_closure(self):
        inputs = generate_model.worker_identity_inputs()
        self.assertIn("//:Cargo.lock", inputs)
        self.assertIn("//crates/lash-vm-client:buildscript_sources", inputs)
        self.assertIn("//crates/lash-vm-worker:buildscript_sources", inputs)

        for package in ("lash-vm-client", "lash-vm-worker"):
            generated = (ROOT / "crates" / package / "BUCK").read_text(
                encoding="utf-8"
            )
            self.assertIn(
                'build_script_env = {"LASH_VM_WORKER_SOURCE_ROOT": ".lash-workspace"}',
                generated,
            )
            for declared in inputs:
                self.assertIn(f'"{declared}"', generated)

        client = (ROOT / "crates/lash-vm-client/BUCK").read_text(encoding="utf-8")
        self.assertIn(
            'extra_srcs = ["//crates/lash-vm-worker:rust_sources"]', client
        )


if __name__ == "__main__":
    unittest.main()
