#!/usr/bin/env python3
"""Run Design 8's existing owning laws once, by full selector, through Kiln."""
import argparse
import collections
import json
from pathlib import Path
import re
import subprocess
import sys


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--rule", action="append", default=[])
    args = parser.parse_args()
    root = Path(__file__).resolve().parents[2]
    rows = json.loads((root / "examples/workflows/laws.json").read_text())
    known = {rule for row in rows for rule in row["rules"]} | {"FEATURE-CUT"}
    unknown = set(args.rule) - known
    if unknown:
        parser.error(f"unknown rules: {', '.join(sorted(unknown))}")
    selected = set(args.rule)
    groups = collections.defaultdict(list)
    for row in rows:
        if not selected or selected.intersection(row["rules"]):
            if row["selector"] not in groups[row["target"]]:
                groups[row["target"]].append(row["selector"])
    evidence = root / ".kiln/FIG-6384/laws"
    evidence.mkdir(parents=True, exist_ok=True)
    summary = []
    for target, selectors in groups.items():
        command = ["kiln", "test", target, "--test_arg=--exact"]
        command.extend(f"--test_arg={selector}" for selector in selectors)
        print(" ".join(command), flush=True)
        name = target.removeprefix("//").replace("/", "-").replace(":", "-")
        log = evidence / f"{name}.log"
        # No shell: selector spelling is preserved, including module paths.
        result = subprocess.run(command, cwd=root, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False)
        log.write_text(result.stdout)
        receipt = re.findall(r"kiln: test (passed|failed) executed=(\d+) passed=(\d+) failed=(\d+) receipt=(\S+)", result.stdout)
        good = False
        if receipt:
            status, executed, passed, failed, path = receipt[-1]
            good = result.returncode == 0 and status == "passed" and int(executed) == len(selectors) and int(passed) == len(selectors) and int(failed) == 0
            print(f"{target}: {status} executed={executed} passed={passed} failed={failed} receipt={path}", flush=True)
        if not good:
            print(f"law target failed or executed the wrong count; see {log}", file=sys.stderr)
            print(result.stdout[-6000:], file=sys.stderr)
        summary.append({"target": target, "selectors": selectors, "ok": good, "log": str(log), "receipt": receipt[-1] if receipt else None})
    if "FEATURE-CUT" in selected:
        fork = root.name
        command = ["kiln", "gate", "lash", fork, "--", "scripts/ci/facade-production.sh"]
        print(" ".join(command), flush=True)
        result = subprocess.run(command, cwd=root, text=True, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, check=False)
        log = evidence / "feature-cut.log"
        log.write_text(result.stdout)
        summary.append({"rule": "FEATURE-CUT", "ok": result.returncode == 0, "log": str(log)})
    (evidence / "results.json").write_text(json.dumps(summary, indent=2) + "\n")
    return 0 if all(row["ok"] for row in summary) else 1


if __name__ == "__main__":
    sys.exit(main())
