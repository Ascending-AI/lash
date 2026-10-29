#!/usr/bin/env python3
"""Regenerate one PostgreSQL artifact. Run shape generation inside a private PG gate."""

from pathlib import Path
import argparse
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
LABEL = "//crates/lash-postgres-store:schema_artifact_generator__bin"


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("artifact", nargs="+", choices=("teardown.sql", "schema-shape.txt"))
    args = parser.parse_args()
    report = ROOT / ".buck2/postgres-artifact-build.json"
    subprocess.run(["kiln", "build", LABEL, "--materializations", "final",
                    "--build-report", str(report)], cwd=ROOT, check=True)
    binary = subprocess.check_output(
        [sys.executable, "tools/buck2/outputs.py", "--report", str(report),
         "--label", LABEL, "--single"], cwd=ROOT, text=True).strip()
    for name in args.artifact:
        artifact = subprocess.check_output([binary, name], cwd=ROOT)
        (ROOT / "crates/lash-postgres-store" / name).write_bytes(artifact)


if __name__ == "__main__":
    main()
