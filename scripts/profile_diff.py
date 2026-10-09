#!/usr/bin/env python3
"""Compare folded profiles, or build and sample two commits in this Kiln fork."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
import subprocess

from perf_artifacts import artifacts, runtime_label
from perf_capture import checked, run_profiled, tool


def differential(before: Path, after: Path, out: Path, *, normalize: bool,
                 svg: bool, top: int) -> str:
    out.mkdir(parents=True, exist_ok=True)
    cmd = [tool("inferno-diff-folded")]
    if normalize:
        cmd.append("--normalize")
    text = checked([*cmd, str(before), str(after)]).stdout
    (out / "diff.folded").write_text(text)
    rows = []
    for line in text.splitlines():
        stack, old, new = line.rsplit(" ", 2)
        old, new = int(old), int(new)
        rows.append((new - old, old, new, stack))
    rows.sort(key=lambda row: (-abs(row[0]), row[3]))
    report = "delta before after stack\n" + "".join(
        f"{delta:+d} {old} {new} {stack}\n" for delta, old, new, stack in rows[:top])
    (out / "diff.top.txt").write_text(report)
    if svg:
        result = checked([tool("inferno-flamegraph"), "--title", "CPU differential",
                          str(out / "diff.folded")])
        (out / "diff.svg").write_text(result.stdout)
    return report


def sample_commits(root: Path, commits: list[str], out: Path,
                   scenario: str, runs: int, turns: int) -> list[Path]:
    if checked(["git", "status", "--porcelain", "--untracked-files=no"], cwd=root).stdout:
        raise SystemExit("error: commit comparison requires a clean tracked tree; commit changes first")
    branch = subprocess.run(["git", "symbolic-ref", "-q", "--short", "HEAD"],
                            cwd=root, capture_output=True, text=True)
    original = branch.stdout.strip() if branch.returncode == 0 else \
        checked(["git", "rev-parse", "HEAD"], cwd=root).stdout.strip()
    revisions = [checked(["git", "rev-parse", "--verify", f"{rev}^{{commit}}"], cwd=root).stdout.strip()
                 for rev in commits]
    folded = []
    try:
        for index, revision in enumerate(revisions):
            checked(["git", "switch", "--detach", revision], cwd=root)
            directory = out / f"{index}-{revision[:12]}"
            directory.mkdir(parents=True, exist_ok=True)
            label = runtime_label([])
            binary = artifacts(root, [label], build=True, report=directory / "build.json",
                               optimized=True, symbolized=True)[label]
            receipt = directory / "runtime.json"
            run_profiled([str(binary), "--runtime-perf-scenario", scenario,
                          f"--runtime-perf-runs={runs}", "--runtime-perf-warmups=0",
                          f"--runtime-perf-turns={turns}", "--runtime-perf-smoke",
                          f"--runtime-perf-out={receipt}"], receipt=receipt, cpu=True, cwd=root)
            folded.append(receipt.with_suffix(".profiles") / "cpu.folded")
        (out / "comparison.json").write_text(json.dumps({
            "commits": revisions, "scenario": scenario, "runs": runs, "turns": turns,
            "folded": [str(path) for path in folded]}, indent=2) + "\n")
    finally:
        restore = ["git", "switch"] + ([] if branch.returncode == 0 else ["--detach"])
        checked([*restore, original], cwd=root)
    return folded


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--folded", nargs=2, type=Path, metavar=("BEFORE", "AFTER"))
    source.add_argument("--commits", nargs=2, metavar=("BEFORE", "AFTER"))
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--scenario", default="standard")
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--turns", type=int, default=1)
    parser.add_argument("--normalize", action="store_true", help="Scale before counts to after's total")
    parser.add_argument("--svg", action="store_true")
    parser.add_argument("--top", type=int, default=30)
    args = parser.parse_args()
    if min(args.runs, args.turns, args.top) < 1:
        parser.error("runs, turns and top must be positive")
    out = args.out.resolve()
    root = Path(__file__).resolve().parents[1]
    try:
        paths = args.folded or sample_commits(root, args.commits, out, args.scenario,
                                             args.runs, args.turns)
        print(differential(*paths, out, normalize=args.normalize, svg=args.svg, top=args.top), end="")
    except (RuntimeError, OSError, ValueError) as error:
        raise SystemExit(f"error: differential failed: {error}") from error
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
