#!/usr/bin/env python3
"""Live, seeded differential fuzzing; failures become minimized corpus cases.

Run outside Buck test actions, after sourcing env.sh:
  (ulimit -v 8388608; /usr/bin/time -v python3 scripts/fuzz-helper-differential.py \
      --cases 10000 --output .kiln/FIG-6372/fuzz)
The machine adapter performs the same comparison as the hermetic corpus law.
"""
import argparse
import json
import os
from pathlib import Path
import shlex
import subprocess
import sys

from helper_differential_common import Generator

ROOT = Path(__file__).resolve().parent.parent


class Peer:
    def __init__(self, command):
        self.process = subprocess.Popen(command, stdin=subprocess.PIPE, stdout=subprocess.PIPE,
                                        text=True, bufsize=1, env={**os.environ, "TZ": "UTC"})

    def call(self, case):
        self.process.stdin.write(json.dumps(case) + "\n")
        self.process.stdin.flush()
        while True:
            line = self.process.stdout.readline()
            if not line:
                raise RuntimeError(f"peer exited with {self.process.poll()}: {self.process.args}")
            if line.startswith("{"):
                answer = json.loads(line)
                if answer.get("id") != case["id"]:
                    raise RuntimeError("peer reordered a case")
                return answer
            # Kiln may print build provenance before starting the adapter.
            print(line.rstrip(), file=sys.stderr)

    def close(self):
        self.process.stdin.close()
        status = self.process.wait(timeout=30)
        if status:
            raise RuntimeError(f"peer exited {status}: {self.process.args}")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cases", type=int, default=10000, help="cases per dialect")
    parser.add_argument("--seed", type=int, default=6372)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--runner-command", default="kiln run //crates/lash-dialect-typescript:helper_differential__example --")
    args = parser.parse_args()
    if args.cases <= 0:
        parser.error("--cases must be positive")
    versions = {"node": subprocess.check_output(["node", "--version"], text=True).strip(), "python": sys.version.split()[0]}
    if versions != {"node": "v25.2.1", "python": "3.12.3"}:
        raise SystemExit(f"Pinned oracles required, got {versions}")
    args.output.mkdir(parents=True, exist_ok=True)
    machine = Peer(shlex.split(args.runner_command))
    summary = {"seed": args.seed, "engines": versions, "dialects": {}}
    total = 0
    try:
        for dialect in ["typescript", "python"]:
            command = ["node", str(ROOT / "scripts/helper-oracle.mjs")] if dialect == "typescript" else [sys.executable, str(ROOT / "scripts/helper_oracle.py")]
            oracle = Peer(command)
            generator = Generator(dialect, args.seed)
            mismatches = 0
            minimized = {}

            def compare(expression, index):
                case = generator.case(expression, index)
                expected = oracle.call(case)["result"]
                observed = machine.call({**case, "reference": expected})
                return observed.get("matches", False), case, expected, observed

            try:
                for index in range(args.cases):
                    if index and index % 1000 == 0:
                        print(f"{dialect}: compared {index} cases, mismatches={mismatches}", flush=True)
                    expression = generator.expression()
                    equal, case, expected, observed = compare(expression, index)
                    if equal:
                        continue
                    mismatches += 1
                    # Greedy structural minimization preserves the result type.
                    # The strictly shorter condition makes termination explicit.
                    while True:
                        for smaller in expression.shrinks():
                            if len(smaller.source) >= len(expression.source):
                                continue
                            match, candidate, reference, actual = compare(smaller, index)
                            if not match:
                                expression, case, expected, observed = smaller, candidate, reference, actual
                                break
                        else:
                            break
                    if case["source"] not in minimized:
                        evidence = {"case": case, "engine": versions["node" if dialect == "typescript" else "python"],
                                    "expected": expected, "actual": observed.get("result"), "error": observed.get("error")}
                        name = f"{dialect}-{index:05d}.json"
                        (args.output / name).write_text(json.dumps(evidence, indent=2, ensure_ascii=True) + "\n")
                        minimized[case["source"]] = name
                        print(f"MISMATCH {name}: {case['source']} expected={expected} actual={observed}", flush=True)
            finally:
                oracle.close()
            summary["dialects"][dialect] = {"executed": args.cases, "mismatches": mismatches, "minimized": list(minimized.values())}
            total += mismatches
            print(f"{dialect}: executed={args.cases} matched={args.cases-mismatches} mismatches={mismatches} minimized={len(minimized)}", flush=True)
    finally:
        machine.close()
        (args.output / "summary.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 1 if total else 0


if __name__ == "__main__":
    raise SystemExit(main())
