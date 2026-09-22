#!/usr/bin/env python3
"""Generate or check the checked-in workflow JSON Schema documents."""

from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess
import sys


ROOT = Path(__file__).resolve().parent.parent
OUTPUT = ROOT / "schemas" / "host"


def command() -> list[str]:
    return [
        "cargo",
        "run",
        "--quiet",
        "--locked",
        "-p",
        "lash-internal-lashlang",
        "--bin",
        "workflow_schema_generator",
    ]


def rendered_documents(payload: list[dict[str, object]]) -> dict[Path, str]:
    documents: dict[Path, str] = {}
    for document in payload:
        shape = document["shape"]
        version = document["version"]
        schema = document["schema"]
        if not isinstance(shape, str) or not isinstance(version, int):
            raise ValueError("generator returned an invalid shape registration")
        relative = Path(shape) / f"v{version}.schema.json"
        documents[relative] = json.dumps(schema, indent=2) + "\n"
    return documents


def generate(check: bool) -> int:
    completed = subprocess.run(
        command(), cwd=ROOT, check=False, capture_output=True, text=True
    )
    if completed.returncode != 0:
        sys.stderr.write(completed.stderr)
        return completed.returncode
    try:
        payload = json.loads(completed.stdout)
        documents = rendered_documents(payload)
    except (json.JSONDecodeError, KeyError, TypeError, ValueError) as error:
        print(f"invalid workflow schema generator output: {error}", file=sys.stderr)
        return 1

    stale: list[str] = []
    for relative, expected in documents.items():
        path = OUTPUT / relative
        if check:
            try:
                actual = path.read_text(encoding="utf-8")
            except OSError as error:
                stale.append(f"{path} cannot be read: {error}")
            else:
                if actual != expected:
                    stale.append(f"{path} differs")
        else:
            path.parent.mkdir(parents=True, exist_ok=True)
            path.write_text(expected, encoding="utf-8")

    expected_paths = {OUTPUT / relative for relative in documents}
    if OUTPUT.exists():
        for path in OUTPUT.glob("*/*.schema.json"):
            if path not in expected_paths:
                if check:
                    stale.append(f"{path} is obsolete")
                else:
                    path.unlink()

    if stale:
        print("workflow schema drift detected:", file=sys.stderr)
        print("\n".join(stale), file=sys.stderr)
        print("run `python3 scripts/generate-workflow-schemas.py`", file=sys.stderr)
        return 1
    if check:
        print("workflow schema documents are current")
    return 0


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument(
        "--check",
        action="store_true",
        help="fail if checked-in documents differ from fresh generation",
    )
    args = parser.parse_args()
    return generate(args.check)


if __name__ == "__main__":
    sys.exit(main())
