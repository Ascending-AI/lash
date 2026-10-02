#!/usr/bin/env python3
"""The SDK worker archive records protocol diagnostics and rejects test/debug helpers."""
import importlib.util
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("package_vm_worker", ROOT / "scripts/package_vm_worker.py")
PACKAGER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGER)


class WorkerBundle(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory(dir=ROOT)
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        subprocess.run(["git", "init", "-q", str(self.root)], check=True)
        for name in ["Cargo.toml", "Cargo.lock", "rust-toolchain.toml", "crates/lash-vm-client/src/lib.rs"]:
            path = self.root / name
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(f"exact contents of {name}\n")
        subprocess.run(["git", "add", "."], cwd=self.root, check=True)
        subprocess.run(["git", "-c", "user.name=Fixture", "-c", "user.email=fixture@example.com", "commit", "-qm", "Fixture"], cwd=self.root, check=True)
        self.worker = self.root / "helper"
        self.write_worker()

    def write_worker(self, **changes):
        info = {"protocol_version": 1, "minimum_supported_protocol_version": 1,
                "crate_version": "0.0.0-dev", "arch": "x86_64", "os": "linux",
                "debug": False, "testing": False} | changes
        self.worker.write_text(f"#!/bin/sh\n[ \"$1\" = --version ] || exit 1\nprintf '%s\\n' '{json.dumps(info)}'\n")
        self.worker.chmod(0o755)

    def test_archive_keeps_exact_source_and_executable(self):
        archive = PACKAGER.package(self.root, self.worker, self.root / "out", "0.0.0-dev", "linux-x86_64")
        with tarfile.open(archive) as tar:
            manifest = json.load(tar.extractfile("manifest.json"))
            self.assertEqual(manifest["worker_sha256"], PACKAGER.digest(self.worker))
            self.assertEqual(manifest["worker"]["protocol_version"], 1)
            self.assertEqual(manifest["worker_binary"], "bin/lash-vm-worker")
            self.assertTrue(tar.getmember("bin/lash-vm-worker").mode & 0o111)
            for name, checksum in manifest["source_sha256"].items():
                self.assertEqual(tar.extractfile(f"sdk/{name}").read(), (self.root / name).read_bytes())
                self.assertEqual(checksum, PACKAGER.digest(self.root / name))
        self.assertEqual(archive.with_suffix(".gz.sha256").read_text(), f"{PACKAGER.digest(archive)}  {archive.name}\n")

    def test_debug_and_testing_builds_are_refused(self):
        for flag in ["debug", "testing"]:
            with self.subTest(flag=flag):
                self.write_worker(**{flag: True})
                with self.assertRaisesRegex(ValueError, "without testing"):
                    PACKAGER.package(self.root, self.worker, self.root / "out", "0.0.0-dev", "linux-x86_64")

    def test_archive_target_comes_from_the_binary(self):
        archive = PACKAGER.package(self.root, self.worker, self.root / "out", "0.0.0-dev")
        self.assertTrue(archive.name.endswith("linux-x86_64.tar.gz"))
        with self.assertRaisesRegex(ValueError, "differs from compiled target"):
            PACKAGER.package(self.root, self.worker, self.root / "out", "0.0.0-dev", "linux-aarch64")

    def test_archive_rejects_non_linux_workers(self):
        self.write_worker(os="freebsd")
        with self.assertRaisesRegex(ValueError, "worker must target Linux"):
            PACKAGER.package(self.root, self.worker, self.root / "out", "0.0.0-dev")
        self.assertFalse((self.root / "out").exists())

    def test_crate_version_is_diagnostic_only(self):
        self.write_worker(crate_version="9.8.7")
        archive = PACKAGER.package(self.root, self.worker, self.root / "out", "0.0.0-dev")
        with tarfile.open(archive) as tar:
            self.assertEqual(json.load(tar.extractfile("manifest.json"))["worker"]["crate_version"], "9.8.7")


if __name__ == "__main__":
    unittest.main()
