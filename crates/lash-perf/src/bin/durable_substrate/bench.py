#!/usr/bin/env python3
"""Run durable-substrate cases on the repository's private, pinned PostgreSQL 18.

Each case gets a fresh cluster and database provisioned with the store's 1.0
schema, under L12a's server settings: fsync, synchronous_commit and
full_page_writes on, pg_stat_statements loaded, 512 connections. Run it through
`kiln gate lash <fork> -- python3 ...`; see README.md beside this file.
"""
from __future__ import annotations

import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import platform
import sys
import tempfile
import time

ROOT = Path(__file__).resolve().parents[5]


def host(binary: Path, runner) -> dict:
    return {
        "platform": platform.platform(),
        "cpu_count": os.cpu_count(),
        "cpu_model": next(
            (line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines()
             if line.startswith("model name")),
            "unknown",
        ),
        "memory_kib": int(Path("/proc/meminfo").read_text().splitlines()[0].split()[1]),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "postgres_settings": runner.SETTINGS,
        "load_average": os.getloadavg(),
        "time": time.strftime("%Y-%m-%dT%H:%M:%S%z"),
    }


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--case", dest="cases", action="append", required=True)
    parser.add_argument("--nodes", type=int, default=1)
    parser.add_argument("bench_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if os.environ.get("LASH_POSTGRES_DATABASE_URL"):
        parser.error("unset LASH_POSTGRES_DATABASE_URL: each case needs its own private server")
    if not os.environ.get("KILN_GATE_ID"):
        parser.error("run with kiln gate lash <fork> -- python3 ...")
    binary = args.binary.resolve()
    evidence = args.evidence_dir.resolve()
    if not evidence.is_relative_to(ROOT):
        parser.error("evidence-dir must be inside this fork")
    evidence.mkdir(parents=True, exist_ok=True)
    tempfile.tempdir = str(evidence)
    spec = importlib.util.spec_from_file_location("pg_runner", ROOT / "tools/buck2/postgres_action_runner.py")
    assert spec is not None and spec.loader is not None
    runner = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(runner)
    runner.DATABASE = os.environ["KILN_GATE_ID"].replace("-", "_")
    runner.SETTINGS.update(
        fsync="on", synchronous_commit="on", full_page_writes="on", max_connections="512",
    )
    bench_args = args.bench_args[1:] if args.bench_args[:1] == ["--"] else args.bench_args
    out = evidence / f"postgres-nodes{args.nodes}.jsonl"
    with (evidence / "host.jsonl").open("a") as log:
        for case in args.cases:
            log.write(json.dumps({"case": case, "nodes": args.nodes, "before": host(binary, runner)}) + "\n")
            log.flush()
            code = runner.run(
                ROOT / ".buck2/native/postgres",
                ROOT / ".buck2/native/nss_wrapper/libnss_wrapper.so",
                ROOT / "crates/lash-postgres-store/schema.sql",
                [str(binary), "--store", "postgres", "--nodes", str(args.nodes), "--case", case,
                 "--out", str(out), *bench_args],
            )
            log.write(json.dumps({"case": case, "nodes": args.nodes, "exit": code,
                                  "load_average_after": os.getloadavg()}) + "\n")
            log.flush()
            if code != 0:
                return code
    return 0


if __name__ == "__main__":
    sys.exit(main())
