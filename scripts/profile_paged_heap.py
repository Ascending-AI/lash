#!/usr/bin/env python3
"""Profile the paged heap's park/resume path and attribute CPU to its sites.

Runs the heap-scaling harness under `perf record` (including the released
workers it spawns), then buckets every folded stack by the innermost frame
that names a persistent-heap site: page faults, sealing, spool I/O,
publication SQL and retirement. The meter's own SQL trace (it expands
every statement to count `sql_expanded_bytes`) is a bucket of its own, so
it never inflates the site whose statement it traced. Writes `attribution.txt` (samples and share
per site, with each site's top stacks) beside perf's `cpu.top.txt` and
`cpu.folded`, and the harness's counted table with wall time beside it.
"""
from __future__ import annotations

import argparse
from collections import Counter, defaultdict
from pathlib import Path
import re
import sys

from perf_capture import run_profiled

# Innermost match wins: a seal that spools is spool I/O, a page read that
# runs SQL is publication SQL only inside the commit. Patterns match
# demangled Rust paths in folded frames.
SITES: list[tuple[str, re.Pattern[str]]] = [
    ("spool I/O", re.compile(r"kernel::pages::spool::|DiskSpool|StageSpool")),
    ("retirement", re.compile(r"retire_step|lash_durable::retirement::|peel_level")),
    ("publication SQL", re.compile(r"install_candidate|commit_candidate|install_nodes|install_edges|apply_owner")),
    ("sealing", re.compile(r"WirePageSource.*seal|Port::flush|Port::take_batch|Port::request|frame_page|paged::tree::.*seal|PagedHeap.*seal|checkpoint::.*seal|pages::Stage.*seal|Stage.*admit")),
    ("page faults", re.compile(r"WirePageSource.*read|Port::accept|Tree.*load_shared|pages::Stage.*read|read_pages|store_read|answer_page")),
    ("meter SQL trace", re.compile(r"observed_sql::trace")),
]


def attribute(folded: Path, top: int) -> tuple[Counter, dict[str, Counter], int]:
    totals: Counter = Counter()
    stacks: dict[str, Counter] = defaultdict(Counter)
    samples = 0
    for line in folded.read_text().splitlines():
        stack, _, count = line.rpartition(" ")
        if not stack or not count.isdigit():
            continue
        weight = int(count)
        samples += weight
        frames = stack.split(";")
        site = "other"
        for frame in reversed(frames):
            hit = next((name for name, pattern in SITES if pattern.search(frame)), None)
            if hit:
                site = hit
                break
        totals[site] += weight
        stacks[site][";".join(frames[-6:])] += weight
    return totals, stacks, samples


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True,
                        help="durable-substrate built for //tools/buck2:profiling")
    parser.add_argument("--worker", type=Path, required=True,
                        help="lash-vm-worker built for //tools/buck2:profiling")
    parser.add_argument("--out", type=Path, required=True, help="fresh evidence directory")
    parser.add_argument("--scenario", default="objects-update")
    parser.add_argument("--heap-sizes", default="1000,10000")
    parser.add_argument("--top", type=int, default=5)
    args = parser.parse_args()
    out = args.out.resolve()
    if out.exists():
        sys.exit(f"{out} exists: use a fresh evidence directory")
    out.mkdir(parents=True)
    command = [str(args.binary.resolve()), "--store", "sqlite", "--sqlite-dir", str(out / "db"),
               "--case", "heap-scaling", "--heap-scenarios", args.scenario,
               "--heap-sizes", args.heap_sizes, "--vm-worker", str(args.worker.resolve()),
               "--out", str(out / "heap.jsonl")]
    proc = run_profiled(command, receipt=out / "heap.json", cpu=True, kernel=True)
    (out / "heap.txt").write_text(proc.stdout)
    folded = out / "heap.profiles" / "cpu.folded"
    totals, stacks, samples = attribute(folded, args.top)
    lines = [f"# heap attribution: {args.scenario} H={args.heap_sizes}; samples={samples} "
             "(user+kernel cycles at 999 Hz, every process and thread)",
             "site samples share"]
    for name in [name for name, _ in SITES] + ["other"]:
        lines.append(f"{name.replace(' ', '_')} {totals[name]} {100 * totals[name] / max(samples, 1):.1f}%")
    for name, _ in SITES:
        lines.append(f"\n## {name}: top stacks (innermost six frames)")
        lines += [f"{count} {stack}" for stack, count in stacks[name].most_common(args.top)] or ["(no samples)"]
    lines.append("\n## counted table (wall_ms/seed_ms diagnostic)")
    lines.append(proc.stdout.rstrip())
    (out / "attribution.txt").write_text("\n".join(lines) + "\n")
    print("\n".join(lines[: 3 + len(SITES)]))
    print(f"attribution: {out / 'attribution.txt'}; folded: {folded}; top-N: {folded.with_name('cpu.top.txt')}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
