#!/usr/bin/env python3
"""Emit stable, independent judged-runbook rows for one shard."""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[1]
MATRIX = ROOT / "runbooks" / "parity-matrix.toml"


def parse_shard(value: str) -> tuple[int, int]:
    try:
        index_text, count_text = value.split("/", 1)
        index, count = int(index_text), int(count_text)
    except (ValueError, TypeError) as error:
        raise argparse.ArgumentTypeError("expected I/N") from error
    if count < 1 or not 1 <= index <= count:
        raise argparse.ArgumentTypeError("expected 1 <= I <= N")
    return index, count


def rows(config: dict[str, object]) -> list[dict[str, str]]:
    result = [
        {
            "scenario": scenario,
            "dialect": dialect,
            "runbook": f"runbooks/{scenario}/runbook.md",
        }
        for scenario in config["scenarios"]
        for dialect in config["dialects"]
    ]
    result.extend(
        {
            "scenario": scenario,
            "dialect": "typescript",
            "runbook": f"runbooks/{scenario}/runbook.md",
        }
        for scenario in config["typescript_only"]
    )
    return result


def select_shard(
    all_rows: list[dict[str, str]], index: int, count: int
) -> list[dict[str, str]]:
    """The rows belonging to shard `index` of `count`, 1-based.

    A function rather than a comprehension inside `main` so the test can drive
    this arithmetic instead of restating it. A test that re-implements the
    split proves Python slicing works and nothing about the script: dropping a
    row here used to leave it green.
    """
    return [row for offset, row in enumerate(all_rows) if offset % count == index - 1]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--shard", type=parse_shard, default=(1, 1), metavar="I/N")
    args = parser.parse_args()
    with MATRIX.open("rb") as handle:
        config = tomllib.load(handle)
    all_rows = rows(config)
    missing = [row["runbook"] for row in all_rows if not (ROOT / row["runbook"]).is_file()]
    if missing:
        print(f"missing runbooks: {', '.join(missing)}", file=sys.stderr)
        return 2
    index, count = args.shard
    selected = select_shard(all_rows, index, count)
    print(
        json.dumps(
            {
                "schema": "lash.judged-runbook-shard.v1",
                "shard": f"{index}/{count}",
                "execution_model_floor": config["execution_model_floor"],
                "judge_model_floor": config["judge_model_floor"],
                "rows": selected,
            },
            indent=2,
            sort_keys=True,
        )
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
