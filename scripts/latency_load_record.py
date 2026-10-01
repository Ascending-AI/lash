#!/usr/bin/env python3
"""Record the host's load beside a latency run, and say whether it qualifies.

    scripts/latency_load_record.py --out latency-load.json -- <command...>

Runs the command, forwarding its output, and samples through the whole run:

* the load averages `uptime` prints, read from `/proc/loadavg`;
* CPU pressure, read from `/proc/pressure/cpu`.

`lash-perf latency` prints `latency case <name>: started` and
`latency case <name>: finished` around each case, so the record knows the
wall-clock window of every case the command ran.

FIG-3843's quiet-host rule: a latency run counts only when the 1-minute load
stays below the core count for the whole fast case. The record's `qualified`
is false when any sample inside the fast case's window reaches the core
count, and also when the rule could not be checked: the fast case never ran
to its end, or no sample fell inside its window. The reason is recorded and
printed as one `latency load record: ...` line.

The exit status is the command's own: an unqualified run is marked, not
failed, because its correctness results still stand.
"""

from __future__ import annotations

import argparse
import json
import os
import re
import subprocess
import sys
import threading
import time
from pathlib import Path

LOADAVG = Path("/proc/loadavg")
CPU_PRESSURE = Path("/proc/pressure/cpu")
CASE_MARKER = re.compile(r"^latency case (?P<name>\S+): (?P<edge>started|finished)\s*$")


def read_loadavg(path: Path) -> dict[str, float]:
    one, five, fifteen = path.read_text(encoding="utf-8").split()[:3]
    return {"load1": float(one), "load5": float(five), "load15": float(fifteen)}


def read_cpu_pressure(path: Path) -> dict[str, float] | None:
    """`some`/`full` averages and totals, or None where the kernel has no PSI."""

    try:
        text = path.read_text(encoding="utf-8")
    except OSError:
        return None
    pressure: dict[str, float] = {}
    for line in text.splitlines():
        kind, *fields = line.split()
        for field in fields:
            key, value = field.split("=", 1)
            pressure[f"{kind}_{key}"] = float(value)
    return pressure


def sample(loadavg: Path, pressure: Path, now: float) -> dict:
    return {"unix_s": now, **read_loadavg(loadavg), "cpu_pressure": read_cpu_pressure(pressure)}


def case_windows(markers: list[dict]) -> dict[str, dict]:
    """Each case's window: its started and finished marker times."""

    windows: dict[str, dict] = {}
    for marker in markers:
        window = windows.setdefault(marker["case"], {"started_unix_s": None, "finished_unix_s": None})
        window[f"{marker['edge']}_unix_s"] = marker["unix_s"]
    return windows


def qualify(samples: list[dict], windows: dict[str, dict], case: str, cores: int) -> dict:
    """The verdict on `case`'s window: load1 below `cores` in every sample."""

    window = windows.get(case)
    verdict = {"case": case, "cores": cores, "window": window, "samples_in_window": 0,
               "max_load1": None, "max_cpu_some_avg10": None}
    if window is None or window["started_unix_s"] is None:
        return {**verdict, "qualified": False, "reason": f"the {case} case did not run"}
    if window["finished_unix_s"] is None:
        return {**verdict, "qualified": False, "reason": f"the {case} case did not finish"}
    inside = [row for row in samples
              if window["started_unix_s"] <= row["unix_s"] <= window["finished_unix_s"]]
    if not inside:
        return {**verdict, "qualified": False,
                "reason": f"no load sample fell inside the {case} case's window"}
    max_load1 = max(row["load1"] for row in inside)
    some = [row["cpu_pressure"]["some_avg10"] for row in inside
            if row["cpu_pressure"] and "some_avg10" in row["cpu_pressure"]]
    verdict.update(samples_in_window=len(inside), max_load1=max_load1,
                   max_cpu_some_avg10=max(some) if some else None)
    if max_load1 >= cores:
        return {**verdict, "qualified": False,
                "reason": f"load1 reached {max_load1:.2f} during the {case} case, "
                          f"at or above the {cores} cores"}
    return {**verdict, "qualified": True,
            "reason": f"load1 stayed below the {cores} cores during the {case} case "
                      f"(max {max_load1:.2f})"}


def record(command: list[str], *, interval: float, case: str, cores: int,
           loadavg: Path, pressure: Path, out) -> tuple[int, dict]:
    """Run `command`, sampling load until it exits. Answers its exit status
    and the record."""

    samples: list[dict] = []
    markers: list[dict] = []
    done = threading.Event()

    def sampler() -> None:
        while True:
            samples.append(sample(loadavg, pressure, time.time()))
            if done.wait(interval):
                break
        samples.append(sample(loadavg, pressure, time.time()))

    started = time.time()
    process = subprocess.Popen(command, stdout=subprocess.PIPE, text=True, errors="replace")
    thread = threading.Thread(target=sampler, daemon=True)
    thread.start()
    assert process.stdout is not None
    for line in process.stdout:
        out.write(line)
        out.flush()
        marker = CASE_MARKER.match(line)
        if marker:
            markers.append({"case": marker["name"], "edge": marker["edge"], "unix_s": time.time()})
    process.stdout.close()
    status = process.wait()
    done.set()
    thread.join()
    windows = case_windows(markers)
    return status, {
        "schema_version": 1,
        "command": command,
        "exit_status": status,
        "started_unix_s": started,
        "finished_unix_s": time.time(),
        "interval_seconds": interval,
        "cases": windows,
        "verdict": qualify(samples, windows, case, cores),
        "samples": samples,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", required=True, type=Path, help="write the load record here")
    parser.add_argument("--interval", type=float, default=1.0, help="seconds between samples")
    parser.add_argument("--case", default="fast", help="the case whose window must be quiet")
    parser.add_argument("--cores", type=int, default=os.cpu_count() or 1,
                        help="the load1 bound (default: this host's core count)")
    parser.add_argument("--loadavg", type=Path, default=LOADAVG, help=argparse.SUPPRESS)
    parser.add_argument("--cpu-pressure", type=Path, default=CPU_PRESSURE, help=argparse.SUPPRESS)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("name the command to run after `--`")
    status, result = record(command, interval=args.interval, case=args.case, cores=args.cores,
                            loadavg=args.loadavg, pressure=args.cpu_pressure, out=sys.stdout)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    verdict = result["verdict"]
    print(f"latency load record: {'QUALIFIED' if verdict['qualified'] else 'UNQUALIFIED'}: "
          f"{verdict['reason']}; {len(result['samples'])} samples in {args.out}", flush=True)
    return status


if __name__ == "__main__":
    sys.exit(main())
