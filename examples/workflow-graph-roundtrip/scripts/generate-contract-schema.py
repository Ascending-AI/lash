#!/usr/bin/env python3
"""Generate or check the example-owned WorkflowDocument JSON Schema."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[3]
OUTPUT = (
    ROOT
    / "examples/workflow-graph-roundtrip/frontend/src/generated/workflow-document.schema.json"
)


def render() -> tuple[int, str]:
    completed = subprocess.run(
        [
            "cargo",
            "run",
            "--quiet",
            "--locked",
            "-p",
            "workflow-graph-roundtrip",
            "--bin",
            "workflow_contract_schema",
        ],
        cwd=ROOT,
        check=False,
        capture_output=True,
        text=True,
    )
    if completed.returncode != 0:
        sys.stderr.write(completed.stderr)
        return completed.returncode, ""
    try:
        schema = json.loads(completed.stdout)
    except json.JSONDecodeError as error:
        print(f"invalid WorkflowDocument schema output: {error}", file=sys.stderr)
        return 1, ""
    return 0, json.dumps(schema, indent=2) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    status, expected = render()
    if status:
        return status
    if args.check:
        actual = OUTPUT.read_text(encoding="utf-8") if OUTPUT.exists() else None
        if actual != expected:
            print(f"generated contract schema is stale: {OUTPUT}", file=sys.stderr)
            print("run `npm run generate:types` in the frontend", file=sys.stderr)
            return 1
        print("example WorkflowDocument schema is current")
        return 0
    OUTPUT.parent.mkdir(parents=True, exist_ok=True)
    OUTPUT.write_text(expected, encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
