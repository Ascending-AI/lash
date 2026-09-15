#!/usr/bin/env python3
"""Take a reproducible runtime-perf baseline and archive it under ``<root>/<sha>/``.

Every wall-clock claim in a performance review is only as good as the box it
was taken on, and a baseline nobody can reproduce is an anecdote. This wraps
``scripts/profile_runtime.py`` with the three things that were previously done
by hand and therefore inconsistently:

1. **A quiet-box precondition.** The run refuses to start when the one-minute
   load average is above ``--max-load`` or when compiler processes are running,
   because a contended box inflates every duration. ``--allow-busy`` records
   the violation in the manifest instead of refusing, for the cases where
   counter metrics are the verdict and wall clock is only context.
2. **Percentile reconstruction**, so the archive carries the p50/p95/p99 table
   next to the raw ledger.
3. **A manifest** naming the commit, the working-tree cleanliness, the exact
   command, the pre- and post-run ``uptime`` lines and the measured wall time,
   so a later reader can tell a real regression from a noisy box.

PostgreSQL is not managed here: wrap the invocation the way the perf workflow
does, e.g.

    scripts/ci/with-service.sh pg16 -- python3 scripts/perf_baseline.py \\
        --archive-root /workspace/notes/lash/perf-campaign-2026-08-23/baselines \\
        --scenario durable_standard_tool_turn_sqlite

The archive layout matches the existing baselines: ``runtime-perf.json``,
``percentiles.md`` and ``MANIFEST.md`` under a directory named for the short
commit SHA.
"""

from __future__ import annotations

import argparse
import datetime as dt
import os
import platform
import shlex
import subprocess
import sys
import time
from pathlib import Path


ROOT = Path(__file__).resolve().parent.parent
_ENV_ARCHIVE_ROOT = os.environ.get("LASH_PERF_BASELINE_ROOT", "").strip()
BUSY_PROCESS_NAMES = ("rustc", "cargo", "bazel", "lash-perf")
DEFAULT_MAX_LOAD = 8.0


class BaselineError(RuntimeError):
    """The baseline cannot be taken as requested."""


def git(*args: str) -> str:
    completed = subprocess.run(
        ["git", *args], cwd=ROOT, capture_output=True, text=True, check=False
    )
    if completed.returncode != 0:
        raise BaselineError(f"git {' '.join(args)} failed: {completed.stderr.strip()}")
    return completed.stdout.strip()


def running_build_processes() -> dict[str, int]:
    """Count live processes whose executable name is a known build hog.

    Reads ``/proc`` directly rather than shelling out to a pattern matcher, so
    the check can never match its own command line.
    """
    counts: dict[str, int] = {}
    proc = Path("/proc")
    if not proc.is_dir():
        return counts
    for entry in proc.iterdir():
        if not entry.name.isdigit():
            continue
        try:
            name = (entry / "comm").read_text().strip()
        except OSError:
            continue
        if name in BUSY_PROCESS_NAMES:
            counts[name] = counts.get(name, 0) + 1
    return counts


def uptime_line() -> str:
    try:
        return subprocess.run(
            ["uptime"], capture_output=True, text=True, check=True
        ).stdout.strip()
    except (OSError, subprocess.CalledProcessError):
        return "unavailable"


def load_average() -> float:
    return os.getloadavg()[0]


def quiet_box_report(max_load: float) -> tuple[bool, list[str]]:
    """Return whether the box is quiet enough, and every reason it is not."""
    violations: list[str] = []
    load = load_average()
    if load > max_load:
        violations.append(f"one-minute load {load:.2f} exceeds the {max_load:.2f} limit")
    # A live `lash-perf` before this run starts means someone else is measuring
    # on the same box; the compilers are the usual wall-clock contaminant.
    for name, count in sorted(running_build_processes().items()):
        violations.append(f"{count} `{name}` process(es) are running")
    return (not violations, violations)


def profile_command(args: argparse.Namespace, out: Path) -> list[str]:
    command = [
        sys.executable,
        str(ROOT / "scripts" / "profile_runtime.py"),
        "--profile",
        args.profile,
        "--runs",
        str(args.runs),
        "--warmups",
        str(args.warmups),
        "--turns",
        str(args.turns),
        "--out",
        str(out),
    ]
    if args.release:
        command.append("--release")
    if args.no_build:
        command.append("--no-build")
    for scenario in args.scenario:
        command.extend(["--scenario", scenario])
    command.extend(args.profile_arg)
    return command


def render_manifest(
    *,
    sha: str,
    dirty: str,
    label: str,
    quiet: bool,
    violations: list[str],
    before_uptime: str,
    after_uptime: str,
    measure_seconds: float,
    percentile_seconds: float,
    commands: list[list[str]],
) -> str:
    lines = [
        f"# Runtime perf baseline — {label}",
        "",
        f"- SHA: `{sha}`",
        f"- Date: `{dt.datetime.now(dt.timezone.utc).strftime('%Y-%m-%d %H:%M:%SZ')}`",
        f"- Host: `{platform.node()}` ({os.cpu_count()} CPUs)",
        f"- Working tree: {'clean' if not dirty else 'DIRTY — the ledger is not attributable to the SHA alone'}",
        f"- Quiet-box precondition: {'met' if quiet else 'NOT met'}",
    ]
    for violation in violations:
        lines.append(f"  - {violation}")
    lines.extend(
        [
            f"- Pre-run uptime: `{before_uptime}`",
            f"- Post-run uptime: `{after_uptime}`",
            f"- Measurement wall time: `{measure_seconds:.3f} s`",
            f"- Percentile-generation wall time: `{percentile_seconds:.3f} s`",
            "",
            "## Commands",
            "",
            "```text",
        ]
    )
    lines.extend(shlex.join(command) for command in commands)
    lines.extend(["```", ""])
    if dirty:
        lines.extend(["## Uncommitted paths at measurement time", "", "```text", dirty, "```", ""])
    return "\n".join(lines)


def parse_args(argv: list[str] | None = None) -> argparse.Namespace:
    parser = argparse.ArgumentParser(
        description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter
    )
    parser.add_argument(
        "--archive-root",
        type=Path,
        default=Path(_ENV_ARCHIVE_ROOT) if _ENV_ARCHIVE_ROOT else None,
        help="directory to archive under; also LASH_PERF_BASELINE_ROOT",
    )
    parser.add_argument(
        "--label",
        default="baseline",
        help="a short name for this archive, used in the manifest heading",
    )
    parser.add_argument(
        "--scenario",
        action="append",
        default=[],
        help="scenario to measure; repeatable, defaults to profile_runtime.py's own default set",
    )
    parser.add_argument("--profile", choices=("full", "quick"), default="full")
    parser.add_argument("--runs", type=int, default=5)
    parser.add_argument("--warmups", type=int, default=1)
    parser.add_argument("--turns", type=int, default=12)
    parser.add_argument("--release", action="store_true", default=True)
    parser.add_argument("--debug", dest="release", action="store_false")
    parser.add_argument("--no-build", action="store_true")
    parser.add_argument(
        "--max-load",
        type=float,
        default=DEFAULT_MAX_LOAD,
        help=f"refuse to measure above this one-minute load (default {DEFAULT_MAX_LOAD})",
    )
    parser.add_argument(
        "--allow-busy",
        action="store_true",
        help="record the quiet-box violation in the manifest instead of refusing",
    )
    parser.add_argument(
        "--profile-arg",
        action="append",
        default=[],
        help="extra argument passed through to profile_runtime.py; repeatable",
    )
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv)
    if args.archive_root is None:
        print(
            "error: --archive-root or LASH_PERF_BASELINE_ROOT is required",
            file=sys.stderr,
        )
        return 2

    try:
        sha = git("rev-parse", "HEAD")
        dirty = git("status", "--porcelain")
    except BaselineError as error:
        print(f"error: {error}", file=sys.stderr)
        return 2

    quiet, violations = quiet_box_report(args.max_load)
    if not quiet and not args.allow_busy:
        for violation in violations:
            print(f"error: {violation}", file=sys.stderr)
        print(
            "error: refusing to measure on a busy box; pass --allow-busy to record "
            "the violation and measure anyway (counters stay valid, wall clock does not)",
            file=sys.stderr,
        )
        return 3

    destination = args.archive_root / sha[:9]
    destination.mkdir(parents=True, exist_ok=True)
    ledger = destination / "runtime-perf.json"
    percentiles = destination / "percentiles.md"

    before_uptime = uptime_line()
    measure = profile_command(args, ledger)
    started = time.monotonic()
    completed = subprocess.run(measure, cwd=ROOT, check=False)
    measure_seconds = time.monotonic() - started
    after_uptime = uptime_line()
    if completed.returncode != 0:
        print(f"error: {shlex.join(measure)} exited {completed.returncode}", file=sys.stderr)
        return completed.returncode

    percentile_command = [
        sys.executable,
        str(ROOT / "scripts" / "runtime_perf_percentiles.py"),
        str(ledger),
        "--out",
        str(percentiles),
    ]
    started = time.monotonic()
    percentile_result = subprocess.run(
        percentile_command, cwd=ROOT, capture_output=True, text=True, check=False
    )
    percentile_seconds = time.monotonic() - started
    if percentile_result.returncode != 0:
        print(f"error: {percentile_result.stderr.strip()}", file=sys.stderr)
        return percentile_result.returncode

    (destination / "MANIFEST.md").write_text(
        render_manifest(
            sha=sha,
            dirty=dirty,
            label=args.label,
            quiet=quiet,
            violations=violations,
            before_uptime=before_uptime,
            after_uptime=after_uptime,
            measure_seconds=measure_seconds,
            percentile_seconds=percentile_seconds,
            commands=[measure, percentile_command],
        )
    )
    print(f"archived {sha[:9]} to {destination}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
