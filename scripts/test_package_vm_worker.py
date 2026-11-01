#!/usr/bin/env python3
"""Worker bundles obey the deployment contract and reject incompatible binaries."""
import hashlib
import importlib.util
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch

ROOT = Path(__file__).resolve().parents[1]
SPEC = importlib.util.spec_from_file_location("package_vm_worker", ROOT / "scripts/package_vm_worker.py")
PACKAGER = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(PACKAGER)


class WorkerBundle(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.root = Path(self.temp.name)
        self.worker = self.root / "helper"
        self.write_worker()
        self.notices = b"Generated dependency license notices\n"
        self.generator = self.enterContext(patch.object(PACKAGER, "generate_notices", return_value=self.notices))
        self.enterContext(patch.object(PACKAGER, "check_glibc"))
        # The executable fixture reports build diagnostics; real ELF strip and
        # ABI validation are also exercised by the release packaging proof.
        real_run = subprocess.run
        self.enterContext(patch.object(
            PACKAGER.subprocess, "run",
            side_effect=lambda command, **kwargs: subprocess.CompletedProcess(command, 0)
            if command[0] == "strip" else real_run(command, **kwargs),
        ))

    def write_worker(self, **changes):
        info = {"protocol_version": 1, "minimum_supported_protocol_version": 1,
                "crate_version": "0.0.0-dev", "arch": "x86_64", "os": "linux",
                "debug": False, "testing": False, "dialects": ["typescript", "python"]} | changes
        self.worker.write_text(f"#!/bin/sh\n[ \"$1\" = --version ] || exit 1\nprintf '%s\\n' '{json.dumps(info)}'\n")
        self.worker.chmod(0o755)

    def test_bundle_has_only_deployment_files_and_their_digests(self):
        archive = PACKAGER.package(self.root, self.worker, self.root / "out", "1.2.3", "linux-x86_64")
        with tarfile.open(archive) as tar:
            self.assertEqual(set(tar.getnames()), {"bin/lash-vm-worker", "THIRD-PARTY-NOTICES", "manifest.json"})
            manifest = json.load(tar.extractfile("manifest.json"))
            for item in [manifest["binary"], manifest["notices"]]:
                self.assertEqual(item["sha256"], hashlib.sha256(tar.extractfile(item["path"]).read()).hexdigest())
            self.assertEqual(tar.extractfile("THIRD-PARTY-NOTICES").read(), self.notices)
            self.assertEqual(manifest["protocol_version"], 1)
            self.assertEqual(manifest["glibc_minimum"], "2.34")
            self.assertEqual(manifest["version"], "1.2.3")
            self.assertEqual(manifest["dialects"], ["python", "typescript"])
            self.assertTrue(tar.getmember("bin/lash-vm-worker").mode & 0o111)
        self.assertEqual(archive.with_suffix(".gz.sha256").read_text(), f"{PACKAGER.digest(archive)}  {archive.name}\n")

    def test_debug_and_testing_builds_are_refused(self):
        for flag in ["debug", "testing"]:
            with self.subTest(flag=flag):
                self.write_worker(**{flag: True})
                with self.assertRaisesRegex(ValueError, "without testing"):
                    PACKAGER.package(self.root, self.worker, self.root / "out", "1.2.3")

    def test_archive_target_comes_from_the_binary(self):
        archive = PACKAGER.package(self.root, self.worker, self.root / "out", "1.2.3")
        self.assertTrue(archive.name.endswith("linux-x86_64-python-typescript.tar.gz"))
        with self.assertRaisesRegex(ValueError, "differs from compiled target"):
            PACKAGER.package(self.root, self.worker, self.root / "out", "1.2.3", "linux-aarch64")

    def test_archive_rejects_non_linux_workers(self):
        self.write_worker(os="freebsd")
        with self.assertRaisesRegex(ValueError, "worker must target Linux"):
            PACKAGER.package(self.root, self.worker, self.root / "out", "1.2.3")
        self.assertFalse((self.root / "out").exists())

    def test_each_dialect_build_gets_its_own_notice_closure(self):
        for dialects in [["python"], ["typescript"], ["python", "typescript"]]:
            with self.subTest(dialects=dialects):
                self.write_worker(dialects=dialects)
                archive = PACKAGER.package(self.root, self.worker, self.root / "out", "1.2.3", dialects=dialects)
                self.assertTrue(archive.name.endswith("-" + "-".join(dialects) + ".tar.gz"))
                self.generator.assert_called_with(self.root, "x86_64-unknown-linux-gnu",
                                                  features=",".join(f"dialect-{d}" for d in dialects))
        with self.assertRaisesRegex(ValueError, "differ from compiled dialects"):
            PACKAGER.package(self.root, self.worker, self.root / "out", "1.2.3", dialects=["python"])


class WorkerSelection(unittest.TestCase):
    def test_glibc_floor_compares_numeric_import_versions_and_rejects_newer_abi(self):
        for symbols, expected in [("foo@GLIBC_2.9 bar@GLIBC_2.34", "2.34"),
                                  ("foo@GLIBC_2.35", None), ("", None)]:
            with self.subTest(symbols=symbols), patch.object(PACKAGER.subprocess, "check_output", return_value=symbols):
                if expected:
                    self.assertEqual(PACKAGER.check_glibc(Path("worker")), expected)
                else:
                    with self.assertRaises(ValueError):
                        PACKAGER.check_glibc(Path("worker"))

    def test_target_selection_follows_features_when_generated_names_change(self):
        with tempfile.TemporaryDirectory() as directory:
            source = Path(directory)
            buck = source / "crates/lash-vm-worker/BUCK"
            buck.parent.mkdir(parents=True)
            buck.write_text('''lash_rust_feature_binary(name="lash-vm-worker__bin__new", crate_features=["dialect-python"])
lash_rust_feature_binary(name="lash-vm-worker__bin__test", crate_features=["dialect-python", "testing"])
''')
            self.assertEqual(PACKAGER.worker_target(source, ["python"]),
                             "//crates/lash-vm-worker:lash-vm-worker__bin__new")
            with self.assertRaises(ValueError):
                PACKAGER.worker_target(source, ["typescript"])


if __name__ == "__main__":
    unittest.main()
