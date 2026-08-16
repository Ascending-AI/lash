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


def row(config: dict[str, object], group: str, scenario: str, dialect: str) -> dict[str, str]:
    """One judged row, carrying the execution tier it is funded at.

    The tier and model travel *on the row* rather than being looked up by the
    runner, because a row's evidence bundle has to record which model produced
    it: a substitution that lives only in the matrix file is a substitution
    nobody can read off the artifacts.
    """
    entry = config[group][scenario]
    return {
        "scenario": scenario,
        "dialect": dialect,
        "runbook": f"runbooks/{scenario}/runbook.md",
        "tier": entry["tier"],
        "model": entry["model"],
    }


def rows(config: dict[str, object]) -> list[dict[str, str]]:
    result = [
        row(config, "scenarios", scenario, dialect)
        for scenario in config["scenarios"]
        for dialect in config["dialects"]
    ]
    result.extend(
        row(config, "typescript_only", scenario, "typescript")
        for scenario in config["typescript_only"]
    )
    # Standard-mode hosts have no RLM session, so they have no dialect to pin
    # and no honest twin: one row each, labelled with the mode.
    result.extend(
        row(config, "standard_mode_only", scenario, "standard")
        for scenario in config["standard_mode_only"]
    )
    return result


def tier_violations(config: dict[str, object]) -> list[str]:
    """Every scenario's tier and model, checked against the tier table.

    A model outside its tier's list is the failure this checks for: a row
    silently funded at a model its tier does not fund is exactly the mislabeled
    evidence the matrix exists to prevent, and it is invisible in a diff that
    only reads the tier word.
    """
    tiers = config["tiers"]
    problems = []
    for group in ("scenarios", "typescript_only", "standard_mode_only", "deterministic_only"):
        for scenario, entry in config[group].items():
            tier = entry.get("tier")
            if tier not in tiers:
                problems.append(f"`{scenario}` has unknown tier `{tier}`")
                continue
            if entry.get("model") not in tiers[tier]:
                problems.append(
                    f"`{scenario}` is tier `{tier}` but names model "
                    f"`{entry.get('model')}`, which the tier does not fund"
                )
            phases = entry.get("deterministic_phases")
            if phases is not None and not isinstance(phases, str):
                problems.append(f"`{scenario}` has a non-string `deterministic_phases`")
    return problems


def select_shard(
    all_rows: list[dict[str, str]], index: int, count: int
) -> list[dict[str, str]]:
    """The rows belonging to shard `index` of `count`, 1-based.

    A function rather than a comprehension inside `main` so the test can drive
    this arithmetic instead of restating it. A test that re-implements the
    split proves Python slicing works and nothing about the script: dropping a
    row here used to leave it green.
    """
    return [item for offset, item in enumerate(all_rows) if offset % count == index - 1]


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--shard", type=parse_shard, default=(1, 1), metavar="I/N")
    args = parser.parse_args()
    with MATRIX.open("rb") as handle:
        config = tomllib.load(handle)
    violations = tier_violations(config)
    if violations:
        print(f"tier violations: {'; '.join(violations)}", file=sys.stderr)
        return 2
    all_rows = rows(config)
    missing = [item["runbook"] for item in all_rows if not (ROOT / item["runbook"]).is_file()]
    if missing:
        print(f"missing runbooks: {', '.join(missing)}", file=sys.stderr)
        return 2
    index, count = args.shard
    selected = select_shard(all_rows, index, count)
    print(
        json.dumps(
            {
                "schema": "lash.judged-runbook-shard.v2",
                "shard": f"{index}/{count}",
                "tiers": config["tiers"],
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
