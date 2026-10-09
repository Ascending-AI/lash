#!/usr/bin/env python3
"""Exercise the workflow graph source guard across the worker boundary."""

from pathlib import Path
import shutil
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
RLM = "crates/lash-protocol-rlm/src/executor/mod.rs"
WORKER = "crates/lash-vm-worker/src/service.rs"
TRACE = "crates/lash-vm-runtime/src/process/trace_map.rs"


class WorkflowGraphModelTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory(dir=ROOT)
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.write("scripts/check-workflow-graph-model.sh", "")
        shutil.copyfile(
            ROOT / "scripts/check-workflow-graph-model.sh",
            self.root / "scripts/check-workflow-graph-model.sh",
        )
        self.write(
            "Cargo.toml",
            '[workspace]\nmembers = ["crates/lash-vm", "crates/lash-typescript"]\n',
        )
        for crate, role in (("lash-vm", "language-neutral"), ("lash-typescript", "front-end")):
            self.write(
                f"crates/{crate}/Cargo.toml",
                f'[package]\nname = "{crate}"\nversion = "0.0.0"\n'
                f'[package.metadata.lash]\nrole = "{role}"\n',
            )
        self.write("crates/lash-vm/src/lib.rs", "pub struct WorkflowGraph {}\n")
        self.write("crates/lash-typescript/src/lib.rs", "")
        self.write(
            RLM,
            "fn trace_main_map(artifact: &lash_vm_client::InspectedArtifact) {\n"
            "    lash_vm_runtime::trace_lashlang_main_map(&artifact.graph)\n}\n",
        )
        self.write(
            WORKER,
            "fn inspect(artifact: &lash_vm::ModuleArtifact) {\n"
            "    lash_vm_client::InspectedArtifact {\n"
            "        graph: lash_vm::workflow_graph_from_artifact(artifact),\n"
            "    }\n}\n",
        )
        self.write(
            TRACE,
            "pub fn trace_lashlang_main_map(graph: &lash_vm::WorkflowGraph) {\n"
            "    trace_workflow_subgraph(&graph.main)\n}\n",
        )
        (self.root / "examples").mkdir()

    def write(self, relative, source):
        path = self.root / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(source)

    def check(self, root=None):
        return subprocess.run(
            ["bash", str((root or self.root) / "scripts/check-workflow-graph-model.sh")],
            capture_output=True,
            text=True,
            check=False,
        )

    def test_worker_projection_and_graph_delegation_pass(self):
        result = self.check()
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_pre_worker_artifact_argument_is_rejected(self):
        path = self.root / RLM
        path.write_text(path.read_text().replace("&artifact.graph", "artifact"))
        self.write(
            TRACE,
            "#[cfg(test)]\nmod tests {\n"
            "    fn fixture() { lash_vm::workflow_graph_from_artifact(artifact); }\n"
            "}\n",
        )
        result = self.check()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("RLM no longer delegates its trace skeleton", result.stderr)

    def test_missing_worker_projection_is_rejected(self):
        self.write(WORKER, "fn inspect() {}\n")
        result = self.check()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("worker no longer projects", result.stderr)

    def test_runtime_test_projection_cannot_replace_worker_projection(self):
        self.write(WORKER, "fn inspect() {}\n")
        with (self.root / TRACE).open("a") as source:
            source.write(
                "#[cfg(test)]\nmod tests {\n"
                "    fn fixture() { lash_vm::workflow_graph_from_artifact(artifact); }\n"
                "}\n"
            )
        result = self.check()
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertIn("worker no longer projects", result.stderr)

    def test_repository_passes(self):
        result = self.check(ROOT)
        self.assertEqual(result.returncode, 0, result.stderr)

    def optional_worker_edge(self, features):
        self.write(
            "Cargo.toml",
            '[workspace]\nmembers = ["crates/lash-vm", "crates/lash-typescript", '
            '"crates/lash-vm-runtime", "crates/lash-vm-worker"]\n',
        )
        self.write(
            "crates/lash-vm-runtime/Cargo.toml",
            '[package]\nname = "lash-vm-runtime"\nversion = "0.0.0"\n'
            '[package.metadata.lash]\nrole = "language-neutral"\n'
            '[dependencies]\nworker = { package = "vm-worker", path = "../lash-vm-worker", optional = true }\n'
            f'[features]\n{features}\n',
        )
        self.write("crates/lash-vm-runtime/src/lib.rs", "")
        self.write(
            "crates/lash-vm-worker/Cargo.toml",
            '[package]\nname = "vm-worker"\nversion = "0.0.0"\n'
            '[dependencies]\nlash-typescript = { path = "../lash-typescript" }\n'
            '[features]\ntesting = []\n',
        )
        self.write("crates/lash-vm-worker/src/lib.rs", "")

    def test_testing_only_optional_worker_edge_passes(self):
        for features in (
            'testing = ["dep:worker", "worker/testing"]',
            'testing = ["dep:worker"]\nproduction = ["worker?/testing"]',
        ):
            with self.subTest(features=features):
                self.optional_worker_edge(features)
                result = self.check()
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_production_reachable_optional_worker_edge_is_rejected(self):
        for features in (
            'testing = ["dep:worker"]\nproduction = ["testing"]',
            'testing = ["dep:worker"]\ndefault = ["testing"]',
            'testing = ["dep:worker"]\nproduction = ["worker/testing"]',
            'testing = ["worker"]',
        ):
            with self.subTest(features=features):
                self.optional_worker_edge(features)
                result = self.check()
                self.assertEqual(result.returncode, 1, result.stderr)
                self.assertIn("language-neutral crate reaches a front end", result.stderr)


if __name__ == "__main__":
    unittest.main()
