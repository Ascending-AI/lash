#!/usr/bin/env python3
"""Dependency-derived VM worker test runfiles."""
import ast
import pathlib
import sys
import unittest
from unittest.mock import patch

ROOT = pathlib.Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/bazel"))
import generate_build_files as generator


class WorkerRunfileGenerationTests(unittest.TestCase):
    def fixture(self, kind=None):
        names = ["host", "middle", "lash-internal-vm-client", "unrelated"]
        packages = [
            {"id": name, "name": name, "manifest_path": str(ROOT / "crates" / name / "Cargo.toml")}
            for name in names
        ]
        metadata = {
            "packages": packages,
            "workspace_members": names,
            "resolve": {"nodes": [
                {"id": "host", "deps": [{"pkg": "middle", "dep_kinds": [{"kind": kind}]}]},
                {"id": "middle", "deps": [{"pkg": names[2], "dep_kinds": [{"kind": None}]}]},
                {"id": names[2], "deps": []},
                {"id": "unrelated", "deps": []},
            ]},
        }
        outputs = {}
        for name in names:
            outputs[ROOT / "crates" / name / "BUILD.bazel"] = (
                f'lash_rust_library(name="{name}", manifest_dir="crates/{name}")\n'
                f'lash_rust_unit_test(\n    name="{name}__unit_test",\n'
                f'    manifest_dir="crates/{name}",\n)\n'
            )
        return metadata, outputs

    def test_check_rejects_a_transitive_spawner_without_its_runfile(self):
        metadata, outputs = self.fixture()
        failures = generator.vm_worker_runfiles.check(metadata, outputs, ROOT)
        self.assertTrue(any("//crates/host:host__unit_test" in failure for failure in failures))

    def test_generation_preserves_other_support_and_is_idempotent(self):
        metadata, outputs = self.fixture()
        path = ROOT / "crates/host/BUILD.bazel"
        outputs[path] = outputs[path].replace(
            '    name="host__unit_test",',
            '    name="host__unit_test",\n    extra_data = ["//:fixture"],\n    test_env = {"OTHER": "value"},',
        )
        generator.vm_worker_runfiles.add(metadata, outputs, ROOT)
        self.assertEqual([], generator.vm_worker_runfiles.check(metadata, outputs, ROOT))
        args = {arg.arg: ast.literal_eval(arg.value) for arg in ast.parse(outputs[path]).body[1].value.keywords}
        self.assertIn("//:fixture", args["extra_data"])
        self.assertEqual("value", args["test_env"]["OTHER"])
        before = dict(outputs)
        generator.vm_worker_runfiles.add(metadata, outputs, ROOT)
        self.assertEqual(before, outputs)
        self.assertNotIn("LASH_VM_WORKER", outputs[ROOT / "crates/unrelated/BUILD.bazel"])

    def test_dev_edges_are_test_local_and_build_edges_are_excluded(self):
        metadata, outputs = self.fixture("dev")
        graph = generator.vm_worker_runfiles.Graph(metadata, outputs, ROOT)
        self.assertTrue(graph.requires("//crates/host:host__unit_test"))
        self.assertFalse(graph.requires("//crates/host:host"))
        metadata, outputs = self.fixture("build")
        self.assertFalse(generator.vm_worker_runfiles.Graph(metadata, outputs, ROOT).requires("//crates/host:host__unit_test"))
        metadata, outputs = self.fixture()
        metadata["resolve"]["nodes"][1]["deps"][0]["dep_kinds"][0]["kind"] = "dev"
        self.assertFalse(generator.vm_worker_runfiles.Graph(metadata, outputs, ROOT).requires("//crates/host:host__unit_test"))

    def test_a_compile_data_helper_is_already_a_runfile(self):
        metadata, outputs = self.fixture()
        path = ROOT / "crates/host/BUILD.bazel"
        outputs[path] = outputs[path].replace(
            '    name="host__unit_test",',
            '    name="host__unit_test",\n    extra_compile_data=["//crates/lash-vm-worker:lash-vm-worker__bin"],',
        )
        generator.vm_worker_runfiles.add(metadata, outputs, ROOT)
        self.assertEqual([], generator.vm_worker_runfiles.check(metadata, outputs, ROOT))
        self.assertNotIn("extra_data", outputs[path])

    def test_feature_variant_swaps_and_extra_dependencies_are_traversed(self):
        metadata, outputs = self.fixture("build")
        path = ROOT / "crates/host/BUILD.bazel"
        outputs[path] += '''lash_rust_feature_test(
    name="variant",
    manifest_dir="crates/host",
    extra_deps={"//crates/middle:variant": "renamed"},
)
'''
        outputs[ROOT / "crates/middle/BUILD.bazel"] += '''lash_rust_feature_library(
    name="variant",
    manifest_dir="crates/middle",
    variant_deps={"//crates/lash-internal-vm-client": "//crates/lash-internal-vm-client:variant"},
)
'''
        outputs[ROOT / "crates/lash-internal-vm-client/BUILD.bazel"] += '''lash_rust_feature_library(
    name="variant",
    manifest_dir="crates/lash-internal-vm-client",
)
'''
        generator.vm_worker_runfiles.add(metadata, outputs, ROOT)
        self.assertEqual([], generator.vm_worker_runfiles.check(metadata, outputs, ROOT))
        self.assertIn("LASH_VM_WORKER", outputs[path])

    def test_synthetic_next_spawners_receive_the_next_helper(self):
        metadata, outputs = self.fixture()
        name = "worker-package"
        metadata["workspace_members"].append(name)
        metadata["packages"].append({"id": name, "name": "lash-internal-vm-worker", "manifest_path": str(ROOT / "crates/lash-vm-worker/Cargo.toml")})
        metadata["resolve"]["nodes"].append({"id": name, "deps": []})
        client = ROOT / "crates/lash-internal-vm-client/BUILD.bazel"
        outputs[client] = outputs[client].replace('name="lash-internal-vm-client",', 'name="lash-internal-vm-client", crate_features=["synthetic-next"],')
        outputs[ROOT / "crates/lash-vm-worker/BUILD.bazel"] = '''lash_rust_library(name="plain", crate_features=[])
lash_rust_feature_library(name="next", crate_features=["synthetic-next"])
lash_rust_binary(name="lash-vm-worker__bin", crate_root="src/main.rs", library=":plain")
lash_rust_feature_binary(name="next__bin", crate_root="src/main.rs", library=":next")
'''
        generator.vm_worker_runfiles.add(metadata, outputs, ROOT)
        self.assertIn("$(rootpath //crates/lash-vm-worker:next__bin)", outputs[ROOT / "crates/host/BUILD.bazel"])
        self.assertEqual([], generator.vm_worker_runfiles.check(metadata, outputs, ROOT))

    def test_check_rejects_the_wrong_environment_and_unsafe_batch_members(self):
        metadata, outputs = self.fixture()
        generator.vm_worker_runfiles.add(metadata, outputs, ROOT)
        path = ROOT / "crates/host/BUILD.bazel"
        outputs[path] = outputs[path].replace("LASH_VM_WORKER", "WRONG_VARIABLE")
        self.assertTrue(generator.vm_worker_runfiles.check(metadata, outputs, ROOT))
        outputs[path] += 'lash_batch_test(name="test_batch", tests=[":host__unit_test"])\n'
        failures = generator.vm_worker_runfiles.check(metadata, outputs, ROOT)
        self.assertTrue(any("batch" in failure for failure in failures))


if __name__ == "__main__":
    unittest.main()
