#!/usr/bin/env python3
"""Record the real engines; only the two *.expected.json files are written."""
import json
import os
from pathlib import Path
import subprocess
import sys

ROOT = Path(__file__).resolve().parent.parent
CORPUS = ROOT / "crates/lash-dialect-typescript/tests/helper_differential"
ENGINES = {"typescript": "v25.2.1", "python": "3.12.3"}


def oracle_command(dialect):
    return ["node", str(ROOT / "scripts/helper-oracle.mjs")] if dialect == "typescript" else [sys.executable, str(ROOT / "scripts/helper_oracle.py")]


def record(cases, dialect):
    version = subprocess.check_output(["node", "--version"], text=True).strip() if dialect == "typescript" else sys.version.split()[0]
    if version != ENGINES[dialect]:
        raise SystemExit(f"{dialect} oracle requires {ENGINES[dialect]}, got {version}")
    selected = [case for case in cases if case["dialect"] == dialect]
    if not selected or len({case["id"] for case in selected}) != len(selected):
        raise SystemExit("corpus needs nonempty, unique case ids")
    output = subprocess.check_output(oracle_command(dialect), input="".join(json.dumps(case) + "\n" for case in selected),
                                     text=True, env={**os.environ, "TZ": "UTC"})
    observations = [json.loads(line) for line in output.splitlines()]
    if [row["id"] for row in observations] != [case["id"] for case in selected]:
        raise SystemExit("oracle lost or reordered a case")
    return {"engine": version, "cases": {row["id"]: row["result"] for row in observations}}


if __name__ == "__main__":
    cases = json.loads((CORPUS / "cases.json").read_text())
    for dialect in ENGINES:
        expected = record(cases, dialect)
        (CORPUS / f"{dialect}.expected.json").write_text(json.dumps(expected, indent=2, ensure_ascii=True) + "\n")
        print(f"{dialect}: recorded {len(expected['cases'])} cases with {expected['engine']}")
