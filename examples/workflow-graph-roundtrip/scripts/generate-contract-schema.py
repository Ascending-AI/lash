#!/usr/bin/env python3
"""Generate or check the example-owned HTTP contract JSON Schemas."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parents[3]
OUTPUTS = {
    shape: ROOT
    / f"examples/workflow-graph-roundtrip/frontend/src/generated/{shape}.schema.json"
    for shape in ("workflow-document", "error-response")
}


def render(shape: str) -> tuple[int, str]:
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
            "--",
            shape,
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
        print(f"invalid {shape} schema output: {error}", file=sys.stderr)
        return 1, ""
    return 0, json.dumps(schema, indent=2) + "\n"


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--check", action="store_true")
    args = parser.parse_args()
    rendered = {}
    for shape in OUTPUTS:
        status, expected = render(shape)
        if status:
            return status
        rendered[shape] = expected
    if args.check:
        stale = [
            output
            for shape, output in OUTPUTS.items()
            if (output.read_text(encoding="utf-8") if output.exists() else None)
            != rendered[shape]
        ]
        if stale:
            for output in stale:
                print(f"generated contract schema is stale: {output}", file=sys.stderr)
            print("run `npm run generate:types` in the frontend", file=sys.stderr)
            return 1
        print("example HTTP contract schemas are current")
        return 0
    for shape, output in OUTPUTS.items():
        output.parent.mkdir(parents=True, exist_ok=True)
        output.write_text(rendered[shape], encoding="utf-8")
    return 0


if __name__ == "__main__":
    sys.exit(main())
