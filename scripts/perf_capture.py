"""Local perf capture with explicit success/failure receipts beside workloads."""
from __future__ import annotations

from collections import Counter
from decimal import Decimal
import json
from pathlib import Path
import re
import shutil
import subprocess
import sys


def tool(name: str) -> str:
    path = shutil.which(name)
    if not path:
        raise RuntimeError(f"required profiling tool is missing from PATH: {name}")
    return path


def checked(cmd: list[str], **kwargs) -> subprocess.CompletedProcess:
    proc = subprocess.run(cmd, capture_output=True, text=True, **kwargs)
    if proc.returncode:
        raise RuntimeError(f"{' '.join(cmd)} exited {proc.returncode}: {proc.stderr.strip()}")
    return proc


def blocked_stacks(text: str) -> tuple[Counter, int]:
    """Pair a sleeping sched_switch stack with that TID's next SWITCH IN.

    Per-task context-switch records supply switch-ins even when the task is not
    running; this avoids system-wide sampling and other processes' stacks.
    Runnable/preempted tasks are excluded. Counts are nanoseconds until resumed
    (including wakeup-to-run latency). Incomplete intervals are counted, omitted.
    """
    header = re.compile(r"^\s*(.*?)\s+(\d+)/\s*(\d+)\s+(\d+\.\d+):\s+(.*)$")
    frame = re.compile(r"^\s+[0-9a-f]+\s+(.+?)\s+\([^)]*\)\s*$")
    sleeping = {}
    totals = Counter()
    pending = None

    def finish():
        nonlocal pending
        if pending:
            tid, stamp, comm, frames = pending
            sleeping[tid] = (stamp, ";".join([comm, *reversed(frames or ["[unknown]"])]))
            pending = None

    for line in text.splitlines():
        if "PERF_RECORD_LOST" in line:
            raise RuntimeError("lost scheduler records; blocked-time profile is incomplete")
        match = header.match(line)
        if match:
            finish()
            comm, _, tid, seconds, event = match.groups()
            stamp = int(Decimal(seconds) * 1_000_000_000)
            if "PERF_RECORD_SWITCH" in event and re.search(r"\bIN\b", event):
                if tid in sleeping:
                    start, stack = sleeping.pop(tid)
                    if stamp < start:
                        raise RuntimeError("off-CPU timestamps are out of order")
                    totals[stack] += stamp - start
            elif "sched:sched_switch:" in event:
                state = re.search(r"prev_state=(\S+)", event)
                prev = re.search(r"prev_pid=(\d+)", event)
                if not state or not prev:
                    raise RuntimeError(f"unrecognized sched_switch record: {line}")
                if state[1] not in ("R", "R+", "0"):
                    pending = (prev[1], stamp, comm.strip(), [])
                else:
                    sleeping.pop(prev[1], None)
        elif pending and (match := frame.match(line)):
            pending[3].append(match[1].replace(";", ":"))
    finish()
    return totals, len(sleeping)


def run_profiled(cmd: list[str], *, receipt: Path, cpu: bool = False,
                 off_cpu: bool = False, population: str | None = None, **kwargs) -> subprocess.CompletedProcess:
    """Run once; capture both populations in the same perf.data when requested."""
    if not cpu and not off_cpu:
        return subprocess.run(cmd, capture_output=True, text=True, **kwargs)
    profile_path = receipt.with_suffix(".profiles")
    if population:
        profile_path = profile_path.with_name(f"{receipt.stem}-{population}.profiles")
    directory = profile_path.resolve()
    directory.mkdir(parents=True, exist_ok=True)
    status_path = directory / "capture.json"
    status = {"status": "failed", "cpu": cpu, "off_cpu": off_cpu, "workload": cmd,
              "workload_receipt": str(receipt.resolve())}
    # Never reuse old successful artifacts after a failed capture.
    for name in ("perf.data", "perf.script", "cpu.folded", "cpu.top.txt",
                 "off-cpu.folded", "off-cpu.top.txt"):
        (directory / name).unlink(missing_ok=True)
    status_path.write_text(json.dumps(status, indent=2) + "\n")
    try:
        perf = tool("perf")
        demangler = tool("c++filt")
        if cpu:
            collapse = tool("inferno-collapse-perf")
        record = [perf, "record", "-g", "--call-graph", "fp", "--no-buildid-cache", "-c", "1",
                  "-o", str(directory / "perf.data")]
        if cpu:
            record += ["-e", "cycles/name=cycles,freq=999/u"]
        if off_cpu:
            record += ["-e", "sched:sched_switch", "--switch-events"]
        proc = subprocess.run([*record, "--", *cmd], capture_output=True, text=True, **kwargs)
        (directory / "record.stderr.txt").write_text(proc.stderr)
        if proc.returncode:
            raise RuntimeError(f"perf record/workload exited {proc.returncode}: {proc.stderr.strip()}")
        script = [perf, "script", "--ns", "--demangle", "-i", str(directory / "perf.data")]
        script += ["-F", "comm,pid,tid,time,event,ip,sym,dso" + (",trace" if off_cpu else "")]
        if off_cpu:
            script += ["--show-switch-events", "--show-lost-events"]
        raw = checked(script, cwd=kwargs.get("cwd")).stdout
        text = checked([demangler, "-s", "rust"], input=raw).stdout
        (directory / "perf.script").write_text(text)
        if cpu:
            folded = checked([collapse, "--event-filter", "cycles",
                              str(directory / "perf.script")]).stdout
            if not folded.strip():
                raise RuntimeError("CPU capture contains no folded samples; increase workload size")
            (directory / "cpu.folded").write_text(folded)
            report = checked([perf, "report", "--stdio", "--no-children", "--demangle",
                              "--sort", "dso,symbol", "--percent-limit", "0",
                              "-g", "none", "-i", str(directory / "perf.data")]).stdout
            report = checked([demangler, "-s", "rust"], input=report).stdout
            # Keep perf's headers and the first 30 rows of the CPU event table.
            rows, count = [], 0
            for line in report.splitlines():
                if "sched:sched_switch" in line:
                    break
                if re.match(r"\s*\d+\.\d+%", line):
                    count += 1
                if count <= 30:
                    rows.append(re.sub(r"\s{2,}", "  ", line.rstrip()))
            (directory / "cpu.top.txt").write_text("\n".join(rows) + "\n")
        if off_cpu:
            totals, incomplete = blocked_stacks(text)
            if not totals:
                raise RuntimeError("no complete blocked intervals; off-CPU capture cannot certify blocked time")
            (directory / "off-cpu.folded").write_text("".join(
                f"{stack} {value}\n" for stack, value in sorted(totals.items())))
            (directory / "off-cpu.top.txt").write_text("blocked_ns stack (top 30)\n" + "".join(
                f"{value} {stack}\n" for stack, value in totals.most_common(30)))
            status["incomplete_intervals_omitted"] = incomplete
        status["status"] = "ok"
        status["units"] = {"cpu": "samples", "off_cpu": "blocked nanoseconds"}
        return proc
    except (OSError, RuntimeError, subprocess.TimeoutExpired) as error:
        status["reason"] = str(error)
        if off_cpu:
            status["privilege_note"] = (
                "sched:sched_switch needs readable tracefs and tracepoint perf permissions; "
                "perf_event_paranoid=1 alone may not permit it. No privilege escalation attempted.")
        raise SystemExit(f"error: profiling failed ({status_path}): {error}") from error
    finally:
        status_path.write_text(json.dumps(status, indent=2) + "\n")
        print(f"Profile capture receipt: {status_path}", file=sys.stderr)
