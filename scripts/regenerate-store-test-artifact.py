#!/usr/bin/env python3
"""Regenerate a store or workflow fixture through its named tooling binary."""

from pathlib import Path
import argparse
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifact", choices=("workflows", "postgres-live-replay-schema-shape.txt",
                                             "postgres-process-replay-schema-shape.txt"))
    artifact = parser.parse_args().artifact
    label = ("//examples/workflows:regenerate__bin" if artifact == "workflows"
             else "//crates/lash-postgres-store:replay_schema_generator__example")
    report = ROOT / ".buck2/store-artifact-build.json"
    subprocess.run(["kiln", "build", label, "--materializations", "final",
                    "--build-report", str(report)], cwd=ROOT, check=True)
    binary = subprocess.check_output(
        [sys.executable, "tools/buck2/outputs.py", "--report", str(report),
         "--label", label, "--single"], cwd=ROOT, text=True).strip()
    if artifact == "workflows":
        subprocess.run([binary, str(ROOT)], cwd=ROOT, check=True)
    else:
        output = subprocess.check_output([binary, artifact], cwd=ROOT)
        (ROOT / "crates/lash" / artifact).write_bytes(output)


if __name__ == "__main__":
    main()
