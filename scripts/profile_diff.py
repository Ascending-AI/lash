#!/usr/bin/env python3
"""Compare folded profiles, or build and sample two commits in this Kiln fork."""
from __future__ import annotations

import argparse
import json
from pathlib import Path
from contextlib import contextmanager
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


@contextmanager
def cell_instrumentation(root: Path, sources: dict[str, bytes]):
    """Apply the current measurement adapter to both revisions; leave the VM intact."""
    saved = {}
    def write(name: str, body: bytes):
        path = root / name
        saved[path] = path.read_bytes() if path.exists() else None
        path.write_bytes(body)
    try:
        for name, body in sources.items():
            write(name, body)
        library = "crates/lash-vm-worker/src/lib.rs"
        text = (root / library).read_text()
        anchor = "    pub mod compare;"
        if anchor not in text:
            raise RuntimeError("base revision predates the worker comparison adapter")
        for module in ("instructions", "cell_profile"):
            if f"pub mod {module};" not in text:
                text = text.replace(anchor, anchor + f"\n    #[cfg(all(target_os = \"linux\", target_arch = \"x86_64\", feature = \"testing\", feature = \"dialect-typescript\", feature = \"dialect-python\"))]\n    pub mod {module};")
        write(library, text.encode())
        main = "crates/lash-vm-worker/src/main.rs"
        text = (root / main).read_text()
        if 'Some("--profile-cell")' not in text:
            allocator = """#[cfg(not(feature = "dhat-heap"))]
#[global_allocator]
static CELL_ALLOCATOR: lash_vm_worker::core_support::cell_profile::CountingAllocator<std::alloc::System> = lash_vm_worker::core_support::cell_profile::CountingAllocator(std::alloc::System);
"""
            entry = """
    if std::env::args().nth(1).as_deref() == Some("--profile-cell") {
        let args: Vec<_> = std::env::args_os().skip(2).collect();
        if args.len() != 2 { eprintln!("usage: --profile-cell FILE OUT"); std::process::exit(2); }
        match lash_vm_worker::core_support::cell_profile::run(std::path::Path::new(&args[0]), std::path::Path::new(&args[1])) {
            Ok(()) => return,
            Err(error) => { eprintln!("cell profile: {error}"); std::process::exit(1); }
        }
    }
"""
            text = allocator + text.replace("fn main() {", "fn main() {" + entry, 1)
        write(main, text.encode())
        yield
    finally:
        for path, original in saved.items():
            if original is None:
                path.unlink(missing_ok=True)
            else:
                path.write_bytes(original)


def sample_commits(root: Path, commits: list[str], out: Path,
                   scenario: str, runs: int, turns: int, *, cell: Path | None = None, worker: bool = False) -> list[Path]:
    if checked(["git", "status", "--porcelain", "--untracked-files=no"], cwd=root).stdout:
        raise SystemExit("error: commit comparison requires a clean tracked tree; commit changes first")
    branch = subprocess.run(["git", "symbolic-ref", "-q", "--short", "HEAD"],
                            cwd=root, capture_output=True, text=True)
    original = branch.stdout.strip() if branch.returncode == 0 else \
        checked(["git", "rev-parse", "HEAD"], cwd=root).stdout.strip()
    revisions = [checked(["git", "rev-parse", "--verify", f"{rev}^{{commit}}"], cwd=root).stdout.strip()
                 for rev in commits]
    folded = []
    cell_reports = []
    if cell:
        from profile_cell import capture
        # Compare VM revisions using one identical measurement adapter, including
        # bases older than this CLI. No VM, helper or dialect code is overlaid.
        instrumentation = {f"crates/lash-vm-worker/src/core_support/{name}.rs":
                           (root / f"crates/lash-vm-worker/src/core_support/{name}.rs").read_bytes()
                           for name in ("compare", "instructions", "cell_profile")}
        # Keep the measured source bytes fixed while the checkout changes.
        source = out / cell.name
        out.mkdir(parents=True, exist_ok=True)
        source.write_bytes(cell.read_bytes())
    try:
        for index, revision in enumerate(revisions):
            checked(["git", "switch", "--detach", revision], cwd=root)
            directory = out / f"{index}-{revision[:12]}"
            directory.mkdir(parents=True, exist_ok=True)
            if cell:
                with cell_instrumentation(root, instrumentation):
                    cell_reports.append(capture(root, source, directory, worker=True))
                folded.append(directory / "joined.folded")
                continue
            label = runtime_label([])
            binary = artifacts(root, [label], build=True, report=directory / "build.json",
                               optimized=True, symbolized=True)[label]
            receipt = directory / "runtime.json"
            run_profiled([str(binary), "--runtime-perf-scenario", scenario,
                          f"--runtime-perf-runs={runs}", "--runtime-perf-warmups=0",
                          f"--runtime-perf-turns={turns}", "--runtime-perf-smoke",
                          f"--runtime-perf-out={receipt}"], receipt=receipt, cpu=True, cwd=root)
            folded.append(receipt.with_suffix(".profiles") / "cpu.folded")
        if cell:
            cell_deltas(cell_reports, out)
        (out / "comparison.json").write_text(json.dumps({
            "commits": revisions, "cell": str(cell) if cell else None, "worker": True if cell else worker, "scenario": scenario, "runs": runs, "turns": turns,
            "folded": [str(path) for path in folded]}, indent=2) + "\n")
    finally:
        restore = ["git", "switch"] + ([] if branch.returncode == 0 else ["--detach"])
        checked([*restore, original], cwd=root)
    return folded


def cell_deltas(reports: list[dict], out: Path) -> None:
    lines = ["Cell CPU sample deltas (after - before); raw counts, matched input"]
    rows = {}
    for label in ("guest", "rust", "joined"):
        before, after = ({row["site"]: row for row in report["tables"][label]} for report in reports)
        rows[label] = [{"site": site, "before_self": before.get(site, {}).get("self", 0),
                        "after_self": after.get(site, {}).get("self", 0),
                        "before_inclusive": before.get(site, {}).get("inclusive", 0),
                        "after_inclusive": after.get(site, {}).get("inclusive", 0)}
                       for site in before.keys() | after.keys()]
        for row in rows[label]:
            row["delta_self"] = row["after_self"] - row["before_self"]
            row["delta_inclusive"] = row["after_inclusive"] - row["before_inclusive"]
        rows[label].sort(key=lambda row: (-abs(row["after_inclusive"] - row["before_inclusive"]), row["site"]))
        lines += [f"\n{label}", "before_self after_self delta_self before_inclusive after_inclusive delta_inclusive site"]
        for row in rows[label][:30]:
            lines.append(f"{row['before_self']} {row['after_self']} {row['after_self']-row['before_self']:+d} "
                         f"{row['before_inclusive']} {row['after_inclusive']} "
                         f"{row['after_inclusive']-row['before_inclusive']:+d} {row['site']}")
    lines.append("\nTotals before / after / delta")
    for key in ("allocations", "charge", "wall_seconds", "samples"):
        before, after = (report["totals"][key] for report in reports)
        lines.append(f"{key}: {before} {after} {after-before:+g}")
    before, after = (report["totals"]["instructions"]["instructions"] for report in reports)
    lines.append(f"instructions:u: {before} {after} {after-before:+d}")
    (out / "cell-delta.json").write_text(json.dumps(rows, indent=2) + "\n")
    text = "\n".join(lines) + "\n"
    (out / "cell-delta.txt").write_text(text)
    print(text, end="")


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument("--folded", nargs=2, type=Path, metavar=("BEFORE", "AFTER"))
    source.add_argument("--commits", nargs=2, metavar=("BEFORE", "AFTER"))
    source.add_argument("--kernel-cost", metavar="PROGRAM", help="Paired kernel phase instruction receipt")
    parser.add_argument("--iterations", type=int, default=100)
    parser.add_argument("--repeats", type=int, default=3)
    parser.add_argument("--build-report", type=Path)
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument("--out", type=Path, required=True)
    parser.add_argument("--cell", type=Path, help="Capture the same source cell at both --commits revisions")
    parser.add_argument("--worker", action="store_true")
    parser.add_argument("--scenario", default="standard")
    parser.add_argument("--runs", type=int, default=1)
    parser.add_argument("--turns", type=int, default=1)
    parser.add_argument("--normalize", action="store_true", help="Scale before counts to after's total")
    parser.add_argument("--svg", action="store_true")
    parser.add_argument("--top", type=int, default=30)
    args = parser.parse_args()
    if min(args.runs, args.turns, args.top, args.iterations, args.repeats) < 1:
        parser.error("runs, turns and top must be positive")
    out = args.out.resolve()
    root = Path(__file__).resolve().parents[1]
    try:
        if args.kernel_cost:
            from profile_kernel_cost import measure
            print(measure(root, out, args.kernel_cost, args.iterations, args.repeats,
                          args.build_report, build=not args.no_build), end="")
            return 0
        paths = args.folded or sample_commits(root, args.commits, out, args.scenario,
                                             args.runs, args.turns, cell=args.cell, worker=args.worker)
        print(differential(*paths, out, normalize=args.normalize, svg=args.svg, top=args.top), end="")
    except (RuntimeError, OSError, ValueError) as error:
        raise SystemExit(f"error: differential failed: {error}") from error
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
