import os
from pathlib import Path
import subprocess
import sys
from types import SimpleNamespace
import re
import unittest

ROOT = Path(__file__).resolve().parents[3]
sys.path.insert(0, str(ROOT / "tools/buck2"))
import vm_worker_runfiles as workers


def starlark_function(path, name):
    source = (ROOT / path).read_text()
    match = re.search(rf'^def {name}\(.*?(?=^def |\Z)', source, re.MULTILINE | re.DOTALL)
    namespace = {}
    exec(match.group(0), namespace)
    return namespace[name]


class TestInputTests(unittest.TestCase):
    """A test sees its declared files at their checkout paths, as under Cargo."""

    def test_every_declared_target_keeps_its_own_resource_name(self):
        resources = starlark_function('tools/buck2/lash_rust.bzl', '_resources')
        named = resources(
            ['tests/fixture.json', 'Cargo.toml'],
            [
                '//crates/lash-vm:rust_sources',
                '//crates/lash-trace:rust_sources',
                'native//:node',
                ':worker__bin',
                '//crates/lash-trace:rust_sources',
            ],
        )
        self.assertEqual(named['tests/fixture.json'], 'tests/fixture.json')
        self.assertEqual(named['Cargo.toml'], 'Cargo.toml')
        self.assertEqual(
            named['__lash_inputs__/crates/lash-vm/rust_sources'],
            '//crates/lash-vm:rust_sources',
        )
        self.assertEqual(
            named['__lash_inputs__/crates/lash-trace/rust_sources'],
            '//crates/lash-trace:rust_sources',
        )
        self.assertEqual(named['__lash_inputs__/native/node'], 'native//:node')
        self.assertEqual(named['__lash_inputs__/worker__bin'], ':worker__bin')
        self.assertEqual(len(named), 6)

    def test_no_test_action_runs_the_env_injecting_run_info(self):
        # A rust_test's RunInfo runs through the prelude's `test_env.json`,
        # which records absolute paths of the host that wrote it. The wrapper
        # and the batch take the test's own command and environment instead.
        wrapper = (ROOT / 'tools/buck2/test_rules.bzl').read_text()
        batch = (ROOT / 'tools/buck2/test_batch.bzl').read_text()
        self.assertNotIn('ctx.attrs.test[RunInfo]', wrapper)
        self.assertIn('"test": attrs.dep(providers = [DefaultInfo, ExternalRunnerTestInfo])', wrapper)
        self.assertNotIn('test[RunInfo]', batch)
        self.assertIn('"members": attrs.list(attrs.dep(providers = [ExternalRunnerTestInfo]))', batch)

    def test_generated_filegroups_link_their_checkout_files(self):
        for path in [ROOT / 'BUCK', ROOT / 'examples/BUCK', *ROOT.glob('*/*/BUCK')]:
            text = path.read_text()
            for block in re.findall(r'^filegroup\(\n.*?^\)', text, re.MULTILINE | re.DOTALL):
                with self.subTest(path=path.relative_to(ROOT).as_posix(), block=block.splitlines()[1]):
                    self.assertIn('    copy = False,\n', block)

    def test_runtime_files_are_exported_by_reference(self):
        text = (ROOT / 'BUCK').read_text()
        for name in ('Cargo.toml', 'Cargo.lock'):
            block = re.search(rf'^export_file\(\n    name = "{re.escape(name)}",\n.*?^\)', text, re.MULTILINE | re.DOTALL)
            self.assertIn('    mode = "reference",\n', block.group(0))


class BinaryRunfileTests(unittest.TestCase):
    def fixture(self, kind=None):
        names = ["host", "middle", workers.SPAWNER_PACKAGE, "unrelated"]
        metadata = {
            "workspace_members": names,
            "packages": [
                {"id": name, "name": name, "manifest_path": str(ROOT / "crates" / name / "Cargo.toml")}
                for name in names
            ],
            "resolve": {"nodes": [
                {"id": "host", "deps": [{"pkg": "middle", "dep_kinds": [{"kind": kind}]}]},
                {"id": "middle", "deps": [{"pkg": workers.SPAWNER_PACKAGE, "dep_kinds": [{"kind": None}]}]},
                {"id": workers.SPAWNER_PACKAGE, "deps": []},
                {"id": "unrelated", "deps": []},
            ]},
        }
        outputs = {
            ROOT / "crates" / name / "BUCK": (
                f'lash_rust_library(name="{name}")\n'
                f'lash_rust_binary(\n    name="{name}__bin",\n'
                f'    library=":{name}",\n)\n'
            ) for name in names
        }
        return metadata, outputs

    def test_binary_dev_and_build_dependencies_do_not_become_runtime_edges(self):
        for kind in ("dev", "build"):
            with self.subTest(kind=kind):
                metadata, outputs = self.fixture(kind)
                workers.add(metadata, outputs, ROOT)
                self.assertNotIn("LASH_VM_WORKER", outputs[ROOT / "crates/host/BUCK"])
        metadata, outputs = self.fixture("dev")
        path = ROOT / "crates/host/BUCK"
        outputs[path] = outputs[path].replace('    name="host__bin",', '    name="host__bin",\n    include_dev_deps=True,')
        workers.add(metadata, outputs, ROOT)
        self.assertIn(workers.WORKER_ENV, outputs[path])

    def test_pruned_optional_dependencies_do_not_require_a_worker(self):
        metadata, outputs = self.fixture()
        metadata["resolve"]["nodes"][0]["deps"][0]["name"] = "optional_middle"
        path = ROOT / "crates/host/BUCK"
        outputs[path] += '''lash_rust_feature_binary(
    name="pruned__bin",
    pruned_deps=["optional_middle"],
)
'''
        workers.add(metadata, outputs, ROOT)
        graph = workers.Graph(metadata, outputs, ROOT)
        binary = graph.targets["//crates/host:pruned__bin"]
        self.assertFalse(graph.requires("//crates/host:pruned__bin"))
        self.assertEqual({}, binary.value("run_env", {}))

    def test_the_worker_uses_its_own_output_without_a_dependency_cycle(self):
        source = (ROOT / 'tools/buck2/run_binary.bzl').read_text()
        namespace = {"native": SimpleNamespace(package_name=lambda: "crates/lash-vm-worker")}
        exec(source[source.index('def binary_run_attrs'):], namespace)
        attrs = namespace['binary_run_attrs'](
            "lash-vm-worker__bin", [workers.WORKER_LABEL, "//:fixture"],
            {"LASH_VM_WORKER": workers.WORKER_ENV, "OTHER": "value"},
        )
        self.assertEqual(["//:fixture"], attrs["extra_data"])
        self.assertEqual({"OTHER": "value"}, attrs["run_env"])
        self.assertEqual(["LASH_VM_WORKER"], attrs["self_env"])

    def test_feature_binary_selects_the_worker_matching_its_client(self):
        metadata, outputs = self.fixture()
        path = ROOT / "crates/host/BUCK"
        outputs[path] = outputs[path].replace(
            '    name="host__bin",',
            '    name="host__bin",\n    run_env={"OTHER": "value"},',
        )
        workers.add(metadata, outputs, ROOT)
        self.assertEqual([], workers.check(metadata, outputs, ROOT))
        graph = workers.Graph(metadata, outputs, ROOT)
        self.assertEqual(
            {"OTHER": "value", "LASH_VM_WORKER": workers.WORKER_ENV},
            graph.targets["//crates/host:host__bin"].value("run_env"),
        )

        metadata, outputs = self.fixture("build")
        path = ROOT / "crates/host/BUCK"
        outputs[path] += '''lash_rust_feature_binary(
    name="next__bin",
    extra_deps={"//crates/lash-internal-vm-client:next": "renamed_client"},
)
'''
        outputs[ROOT / "crates" / workers.SPAWNER_PACKAGE / "BUCK"] += '''lash_rust_feature_library(
    name="next",
    crate_features=["synthetic-next", "testing"],
)
'''
        metadata["workspace_members"].append("worker")
        metadata["packages"].append({"id": "worker", "name": "lash-internal-vm-worker", "manifest_path": str(ROOT / "crates/lash-vm-worker/Cargo.toml")})
        outputs[ROOT / "crates/lash-vm-worker/BUCK"] = '''lash_rust_library(name="plain", crate_features=[])
lash_rust_feature_library(name="next", crate_features=["synthetic-next", "testing"])
lash_rust_binary(name="lash-vm-worker__bin", crate_root="src/main.rs", library=":plain")
lash_rust_feature_binary(name="next__bin", crate_root="src/main.rs", library=":next")
'''
        workers.add(metadata, outputs, ROOT)
        self.assertEqual([], workers.check(metadata, outputs, ROOT))
        graph = workers.Graph(metadata, outputs, ROOT)
        binary = graph.targets["//crates/host:next__bin"]
        self.assertEqual({"LASH_VM_WORKER": "$(location //crates/lash-vm-worker:next__bin)"}, binary.value("run_env"))


class BinaryLauncherTests(unittest.TestCase):
    def run_launcher(self, override=None):
        env = dict(os.environ)
        env.pop("LASH_VM_WORKER", None)
        if override is not None:
            env["LASH_VM_WORKER"] = override
        return subprocess.run(
            ["bash", str(ROOT / "tools/buck2/run_binary.sh"), "LASH_VM_WORKER", "worker path 'quoted' $literal", "--",
             sys.executable, "-c", 'import os, sys; print(repr(os.environ["LASH_VM_WORKER"])); print(repr(sys.argv[1:])); sys.exit(7)',
             "argument with spaces", "$literal"],
            env=env, capture_output=True, text=True, check=False,
        )

    def test_absent_override_uses_the_runfile_and_preserves_arguments_and_exit_status(self):
        result = self.run_launcher()
        self.assertEqual(7, result.returncode, result.stderr)
        self.assertEqual([repr("worker path 'quoted' $literal"), repr(["argument with spaces", "$literal"])], result.stdout.splitlines())

    def test_explicit_override_including_empty_values_wins(self):
        for value in ("/explicit worker 'path'", ""):
            with self.subTest(value=value):
                result = self.run_launcher(value)
                self.assertEqual(7, result.returncode, result.stderr)
                self.assertEqual(repr(value), result.stdout.splitlines()[0])


if __name__ == '__main__':
    unittest.main()
