#!/usr/bin/env python3
"""Run the S2 SQL spike on the repository's private, pinned PostgreSQL 18.

Build first, then run through kiln gate; see the lash-perf substrate README.
The default enables durability on the same test server. --test-settings keeps
its inexpensive fsync-off settings for comparison. No external database is used.
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

ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--binary", type=Path, required=True)
    parser.add_argument("--evidence-dir", type=Path, required=True)
    parser.add_argument("--test-settings", action="store_true")
    parser.add_argument("bench_args", nargs=argparse.REMAINDER)
    args = parser.parse_args()
    if os.environ.get("LASH_POSTGRES_DATABASE_URL"):
        parser.error("unset LASH_POSTGRES_DATABASE_URL: this spike requires its own private server")
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
    if not args.test_settings:
        runner.SETTINGS.update(fsync="on", synchronous_commit="on", full_page_writes="on")
    print(json.dumps({"host": {
        "platform": platform.platform(),
        "cpu_count": os.cpu_count(),
        "cpu_model": next((line.split(":", 1)[1].strip() for line in Path("/proc/cpuinfo").read_text().splitlines() if line.startswith("model name")), "unknown"),
        "memory_kib": int(Path("/proc/meminfo").read_text().splitlines()[0].split()[1]),
        "binary_sha256": hashlib.sha256(binary.read_bytes()).hexdigest(),
        "profile": "test" if args.test_settings else "durable",
        "postgres_settings": runner.SETTINGS,
    }}), flush=True)
    bench_args = args.bench_args[1:] if args.bench_args[:1] == ["--"] else args.bench_args
    return runner.run(
        ROOT / ".buck2/native/postgres",
        ROOT / ".buck2/native/nss_wrapper/libnss_wrapper.so",
        ROOT / "crates/lash-postgres-store/schema.sql",
        [str(binary), *bench_args],
    )


if __name__ == "__main__":
    sys.exit(main())
