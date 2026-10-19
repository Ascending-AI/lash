#!/usr/bin/env python3
"""Record the host's load beside a latency run.

    scripts/latency_load_record.py --out latency-load.json -- <command...>

Runs the command, forwarding its output, and samples through the whole run:

* the load averages `uptime` prints, read from `/proc/loadavg`;
* CPU pressure, read from `/proc/pressure/cpu`.

`lash-perf latency` prints `latency case <name>: started` and
`latency case <name>: finished` around each case, so the record knows the
wall-clock window of every case the command ran, and summarizes the load
inside each window. Both case edges are sampled as well as the interval.

The record is evidence, never a verdict (Sam, 2026-10-19: no load gate on
performance checks): the command's exit status is the wrapper's exit status,
whatever the load was. The latency report's own verdict decides the gate.
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


def window_load(samples: list[dict], window: dict) -> dict:
    """The load inside one case window: its samples, maximum and mean load1,
    and maximum CPU `some` avg10. Empty for a case that never finished."""

    start, end = window["started_unix_s"], window["finished_unix_s"]
    inside = [row for row in samples
              if start is not None and end is not None and start <= row["unix_s"] <= end]
    some = [row["cpu_pressure"]["some_avg10"] for row in inside
            if row["cpu_pressure"] and "some_avg10" in row["cpu_pressure"]]
    return {
        "samples": len(inside),
        "interval_samples": sum(row.get("source") == "interval" for row in inside),
        "max_load1": max((row["load1"] for row in inside), default=None),
        "mean_load1": sum(row["load1"] for row in inside) / len(inside) if inside else None,
        "max_cpu_some_avg10": max(some) if some else None,
    }


def record(command: list[str], *, interval: float, loadavg: Path, pressure: Path,
           out) -> tuple[int, dict]:
    """Run `command`, sampling load until it exits. Return its exit status
    and the record."""

    samples: list[dict] = []
    markers: list[dict] = []
    done = threading.Event()

    def sampler() -> None:
        while True:
            samples.append({**sample(loadavg, pressure, time.time()), "source": "interval"})
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
            edge = {"case": marker["name"], "edge": marker["edge"], "unix_s": time.time()}
            # Both edges are sampled, even when a case is shorter than the
            # interval. Keep their common timestamp inside the case window.
            markers.append(edge)
            samples.append({**sample(loadavg, pressure, edge["unix_s"]),
                            "source": "case-edge", "case": edge["case"], "edge": edge["edge"]})
    process.stdout.close()
    status = process.wait()
    done.set()
    thread.join()
    windows = case_windows(markers)
    samples.sort(key=lambda row: row["unix_s"])
    for window in windows.values():
        window["load"] = window_load(samples, window)
    return status, {
        "schema_version": 2,
        "command": command,
        "exit_status": status,
        "started_unix_s": started,
        "finished_unix_s": time.time(),
        "interval_seconds": interval,
        "cores": os.cpu_count(),
        "cases": windows,
        "samples": samples,
    }


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--out", required=True, type=Path, help="write the load record here")
    parser.add_argument("--interval", type=float, default=1.0, help="seconds between samples")
    parser.add_argument("--loadavg", type=Path, default=LOADAVG, help=argparse.SUPPRESS)
    parser.add_argument("--cpu-pressure", type=Path, default=CPU_PRESSURE, help=argparse.SUPPRESS)
    parser.add_argument("command", nargs=argparse.REMAINDER)
    args = parser.parse_args(argv)
    command = args.command[1:] if args.command[:1] == ["--"] else args.command
    if not command:
        parser.error("name the command to run after `--`")
    status, result = record(command, interval=args.interval, loadavg=args.loadavg,
                            pressure=args.cpu_pressure, out=sys.stdout)
    args.out.parent.mkdir(parents=True, exist_ok=True)
    args.out.write_text(json.dumps(result, indent=2) + "\n", encoding="utf-8")
    for name, window in result["cases"].items():
        load = window["load"]
        print(f"latency load record: case {name}: max load1 {load['max_load1']}, "
              f"max CPU some avg10 {load['max_cpu_some_avg10']}, {load['samples']} samples",
              flush=True)
    print(f"latency load record: {len(result['samples'])} samples in {args.out}", flush=True)
    return status


if __name__ == "__main__":
    sys.exit(main())
