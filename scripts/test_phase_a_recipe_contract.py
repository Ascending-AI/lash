#!/usr/bin/env python3
"""Exercise Phase A's worker staging without compiling or starting services."""

from __future__ import annotations

import json
from pathlib import Path
import shutil
import subprocess
import tempfile
import textwrap
import tomllib
import unittest


ROOT = Path(__file__).resolve().parents[1]


class PhaseARecipeTests(unittest.TestCase):
    def test_stages_both_worker_protocols_in_one_buck2_build(self) -> None:
        justfile = (ROOT / "justfile").read_text()
        recipe = textwrap.dedent(
            justfile.split("_upgrade-harness-builds artifacts:\n", 1)[1]
            .split("\n# Phase A", 1)[0]
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for path in (
                "scripts/resolve_buck2_target.py",
                "tools/buck2/outputs.py",
                "tools/buck2/target-inventory.json",
            ):
                destination = root / path
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copyfile(ROOT / path, destination)
            build = root / "scripts/hermetic-build.sh"
            build.write_text(textwrap.dedent("""\
                #!/usr/bin/env python3
                import json
                from pathlib import Path
                import sys

                args = sys.argv[1:]
                root = Path.cwd()
                with (root / "builds.jsonl").open("a") as log:
                    log.write(json.dumps(args) + "\\n")
                results = {}
                for index, label in enumerate(arg for arg in args if arg.startswith("//")):
                    output = root / f"output-{index}"
                    output.write_text(label)
                    results["root" + label] = {
                        "success": "SUCCESS", "outputs": {"DEFAULT": [str(output)]},
                    }
                report = Path(args[args.index("--build-report") + 1])
                report.write_text(json.dumps({"project_root": str(root), "results": results}))
                """))
            build.chmod(0o755)
            artifacts = root / "artifacts"
            completed = subprocess.run(
                ["bash", "-c", recipe.replace("{{repo}}", str(root)).replace(
                    "{{artifacts}}", str(artifacts)
                )],
                capture_output=True, text=True, check=False,
            )
            self.assertEqual(0, completed.returncode, completed.stderr)
            calls = [json.loads(line) for line in (root / "builds.jsonl").read_text().splitlines()]
            self.assertEqual(1, len(calls))
            self.assertEqual("build", calls[0][0])
            worker_n = artifacts / "bin/n/lash-vm-worker"
            worker_next = artifacts / "bin/n+1/lash-vm-worker"
            self.assertTrue(worker_next.is_file(), "N+1 must stage its Buck2 worker")
            self.assertEqual("//crates/lash-vm-worker:lash-vm-worker__bin", worker_n.read_text())
            resolved = subprocess.check_output(
                ["python3", str(root / "scripts/resolve_buck2_target.py"),
                 "//crates/lash-vm-worker", "lash-vm-worker__bin",
                 "--feature", "synthetic-next", "--feature", "testing"], text=True,
            ).strip()
            self.assertEqual(resolved, worker_next.read_text())
            self.assertIn(worker_n.read_text(), calls[0])
            self.assertIn(resolved, calls[0])
        manifest = tomllib.loads((ROOT / "crates/lash-upgrade-harness/Cargo.toml").read_text())
        self.assertIn("lash-vm-client/synthetic-next", manifest["features"]["synthetic-next"])
        phase_a = justfile.split("phase-a *legs:\n", 1)[1].split(
            "\nagent-workbench-attachment-usage-gate", 1
        )[0]
        self.assertNotIn("cargo build", phase_a)
        self.assertNotIn("--no-run", phase_a)
        self.assertIn("-- cargo test --locked -p lash-upgrade-harness --test phase_a", phase_a)


if __name__ == "__main__":
    unittest.main()
