#!/usr/bin/env python3
"""Read a release corpus explicitly, then run its store semantics. FIG-4495 cut tooling."""

import argparse
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import sys

from verify_release_fixtures import ROOT, VerificationError, verify


class ReadBackError(ValueError):
    """A retained fixture cannot be read."""


def read_corpus(corpus: Path, repo: Path = ROOT) -> int:
    try:
        paths = verify(corpus, repo)
        for path in paths:
            if path.suffix == ".json":
                json.loads(path.read_bytes())
            elif path.suffix == ".db":
                # The corpus is immutable. Store semantics run against copies.
                with sqlite3.connect(path.resolve().as_uri() + "?mode=ro&immutable=1", uri=True) as db:
                    if db.execute("PRAGMA integrity_check").fetchall() != [("ok",)]:
                        raise ReadBackError(f"{path.name}: SQLite integrity check failed")
            elif path.suffix in (".sql", ".md"):
                if not path.read_text(encoding="utf-8").strip():
                    raise ReadBackError(f"{path.name}: empty text fixture")
            else:
                raise ReadBackError(f"{path.name}: unregistered fixture encoding")
    except (VerificationError, OSError, ValueError, sqlite3.Error) as error:
        raise ReadBackError(str(error)) from error
    print(f"read back {len(paths)} fixtures from {corpus}", flush=True)
    return len(paths)


def main(argv=None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("corpus", type=Path)
    parser.add_argument("--runs-per-test", type=int, default=1)
    args = parser.parse_args(argv)
    if args.runs_per_test < 1:
        parser.error("--runs-per-test must be positive")
    if not os.environ.get("KILN_GATE_ID") or not os.environ.get("LASH_POSTGRES_DATABASE_URL"):
        parser.error("run just release-fixtures-read-back inside kiln gate on owned pg")
    try:
        for run in range(1, args.runs_per_test + 1):
            count = read_corpus(args.corpus)
            print(f"corpus read-back run {run}/{args.runs_per_test}: {count} fixtures", flush=True)
        for package, law in (
            ("sqlite", "release_sqlite_fixture_reads_retained_semantics"),
            ("postgres", "release_postgres_fixture_reads_retained_semantics"),
        ):
            subprocess.run([
                "kiln", "test", f"//crates/lash-{package}-store:durable_read_fixture__test",
                "--test_arg=--ignored", "--test_arg=--exact", f"--test_arg={law}",
                "--test_arg=--nocapture", "--nocache_test_results",
                "--local-test-execution",
                f"--runs_per_test={args.runs_per_test}", "--test_sharding_strategy=disabled",
                f"--test_env=LASH_RELEASE_FIXTURES_DIR={args.corpus.resolve()}",
                f"--test_env=LASH_POSTGRES_DATABASE_URL={os.environ['LASH_POSTGRES_DATABASE_URL']}",
            ], cwd=ROOT, check=True)
    except (ReadBackError, subprocess.CalledProcessError) as error:
        print(f"read-release-fixtures error: {error}", file=sys.stderr)
        return 2
    print(f"release fixture store semantics: {2 * args.runs_per_test} laws executed", flush=True)
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
