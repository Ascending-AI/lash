#!/usr/bin/env python3
"""FIG-5021: every launcher binary has an executable VM worker at admission."""

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest


ROOT = Path(__file__).resolve().parents[1]
HOST = "//examples/agent-workbench:agent-workbench"
WORKER = "//crates/lash-vm-worker:lash-vm-worker__bin"


class WorkbenchWorkerTests(unittest.TestCase):
    def setUp(self):
        evidence = ROOT / ".kiln/FIG-5021"
        evidence.mkdir(parents=True, exist_ok=True)
        self.fixture = tempfile.TemporaryDirectory(dir=evidence)
        self.addCleanup(self.fixture.cleanup)
        self.root = Path(self.fixture.name)
        self.bin = self.root / "bin"
        self.bin.mkdir()
        self.runtime = self.root / "runtime"
        self.runtime.mkdir(mode=0o700)
        self.host = self.executable("host", "host-build")
        self.worker = self.executable("worker", "worker-build")
        self.build_args = self.root / "build-args.json"
        mock = self.bin / "kiln"
        mock.write_text(
            "#!/usr/bin/env python3\n"
            "import json, os, pathlib, sys\n"
            "args = sys.argv[1:]\n"
            "pathlib.Path(os.environ['BUILD_ARGS']).write_text(json.dumps(args))\n"
            "report = args[args.index('--build-report') + 1]\n"
            "labels = [arg for arg in args if arg.startswith('//')]\n"
            "outputs = {'root' + label: {'success': 'SUCCESS', 'outputs': {'DEFAULT': "
            "[os.environ['BUILT_WORKER'] if 'lash-vm-worker' in label "
            "else os.environ['BUILT_HOST']]}} for label in labels}\n"
            "pathlib.Path(report).write_text(json.dumps({'project_root': "
            "os.environ['FIXTURE_ROOT'], 'results': outputs}))\n"
        )
        mock.chmod(0o700)

    def executable(self, name, value):
        path = self.root / name
        path.write_text(f"#!/bin/sh\nprintf '%s\\n' '{value}'\n")
        path.chmod(0o700)
        return path

    def prepare(self, **environment):
        source = (ROOT / "scripts/agent-workbench-dev.sh").read_text()
        # Exercise the production preparation boundary without starting services
        # or entering the machine-wide launcher ownership namespace.
        prepare = source[source.index("workbench_buck2_label="):
                         source.index("\nstart_detached()")]
        ownership = source[source.index("private_owned_directory()"):
                           source.index("\nstable_launcher_runtime_root()")]
        script = (
            "set -euo pipefail\n"
            "log() { printf '%s\\n' \"$*\" >&2; }\n"
            "die() { log \"$*\"; exit 1; }\n"
            'repo_root="$REPO_ROOT"\nlauncher_lock_root="$RUNTIME_ROOT"\n'
            "launcher_lock_hash=fixture\n"
            + ownership + prepare
            + '\nprepare_workbench_binary\nprintf "%s\\n" "$workbench_bin"\n'
        )
        env = {key: value for key, value in os.environ.items()
               if key not in {"AGENT_WORKBENCH_BIN", "LASH_VM_WORKER",
                              "AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO"}}
        env.update(PATH=f"{self.bin}:{env['PATH']}", REPO_ROOT=str(ROOT),
                   RUNTIME_ROOT=str(self.runtime), FIXTURE_ROOT=str(self.root),
                   BUILT_HOST=str(self.host), BUILT_WORKER=str(self.worker),
                   BUILD_ARGS=str(self.build_args))
        env.update(environment)
        return subprocess.run(["bash"], input=script, text=True, env=env,
                              capture_output=True, timeout=10)

    def test_judged_build_stages_host_and_worker_together(self):
        result = self.prepare()
        self.assertEqual(result.returncode, 0, result.stderr)
        args = json.loads(self.build_args.read_text())
        self.assertEqual([arg for arg in args if arg.startswith("//")],
                         [HOST, WORKER])
        self.assertEqual(args.count("--build-report"), 1)
        host = Path(result.stdout.strip())
        worker = host.with_name("lash-vm-worker")
        self.assertEqual(host.read_bytes(), self.host.read_bytes())
        self.assertEqual(worker.read_bytes(), self.worker.read_bytes())
        self.assertTrue(os.access(worker, os.X_OK))
        self.assertFalse(list(host.parent.glob(".*")))

    def test_prebuilt_host_without_worker_is_refused_before_launch(self):
        result = self.prepare(AGENT_WORKBENCH_BIN=str(self.host))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("lash-vm-worker", result.stderr)
        self.assertIn("LASH_VM_WORKER", result.stderr)
        self.assertFalse(self.build_args.exists())

    def test_prebuilt_host_accepts_sibling_or_explicit_worker_without_build(self):
        sibling = self.host.with_name("lash-vm-worker")
        sibling.write_bytes(self.worker.read_bytes())
        sibling.chmod(0o700)
        result = self.prepare(AGENT_WORKBENCH_BIN=str(self.host))
        self.assertEqual(result.returncode, 0, result.stderr)
        sibling.unlink()
        result = self.prepare(AGENT_WORKBENCH_BIN=str(self.host),
                              LASH_VM_WORKER=str(self.worker))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse(self.build_args.exists())

    def test_invalid_explicit_worker_is_refused_even_with_sibling(self):
        sibling = self.host.with_name("lash-vm-worker")
        sibling.write_bytes(self.worker.read_bytes())
        sibling.chmod(0o700)
        result = self.prepare(AGENT_WORKBENCH_BIN=str(self.host),
                              LASH_VM_WORKER=str(self.root / "missing"))
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("LASH_VM_WORKER", result.stderr)
        self.assertFalse(self.build_args.exists())


if __name__ == "__main__":
    unittest.main()
