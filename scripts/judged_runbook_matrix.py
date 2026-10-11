#!/usr/bin/env python3
"""Emit stable, independent judged-runbook rows for one shard."""

from __future__ import annotations

import argparse
import json
import pathlib
import sys
import tomllib


ROOT = pathlib.Path(__file__).resolve().parents[1]
MATRIX = ROOT / "runbooks" / "judged-matrix.toml"


def parse_shard(value: str) -> tuple[int, int]:
    try:
        index_text, count_text = value.split("/", 1)
        index, count = int(index_text), int(count_text)
    except (ValueError, TypeError) as error:
        raise argparse.ArgumentTypeError("expected I/N") from error
    if count < 1 or not 1 <= index <= count:
        raise argparse.ArgumentTypeError("expected 1 <= I <= N")
    return index, count


def row(config: dict[str, object], group: str, scenario: str, label: str) -> dict[str, str]:
    """One judged row, carrying its artifact label and the tier it is funded at.

    `label` is the row's artifact directory and evidence claim: a declared
    dialect says an RLM session served that dialect (ADR 0139); `standard`
    says the scenario opened none.

    The tier and model travel *on the row* rather than being looked up by the
    runner, because a row's evidence bundle has to record which model produced
    it: a substitution that lives only in the matrix file is a substitution
    nobody can read off the artifacts.
    """
    entry = config[group][scenario]
    result = {
        "scenario": scenario,
        "label": label,
        "runbook": f"runbooks/{scenario}/runbook.md",
        "tier": entry["tier"],
        "model": entry["model"],
    }
    pending = entry.get("pending_dialects", {}).get(label)
    if pending:
        result["pending"] = pending
    return result


def rows(config: dict[str, object]) -> list[dict[str, str]]:
    result = []
    for group, policy in config["groups"].items():
        if not policy["emits"]:
            continue
        for scenario, entry in config[group].items():
            labels = (
                entry.get("dialects", ["typescript"])
                if policy["label"] == "dialect"
                else [policy["label"]]
            )
            result.extend(row(config, group, scenario, label) for label in labels)
    return result


def dialect_violations(config: dict[str, object]) -> list[str]:
    """A dialect label must name one declared host dialect, exactly once."""
    declared = config["dialects"]
    problems = []
    for group, policy in config["groups"].items():
        for scenario, entry in config[group].items():
            if policy.get("label") == "standard":
                if "dialects" in entry:
                    problems.append(f"`{scenario}` opens no RLM session but declares dialects")
                continue
            if policy.get("referent") in ("harness", "runbook"):
                continue
            dialects = entry.get("dialects", ["typescript"])
            pending = entry.get("pending_dialects", {})
            if not isinstance(pending, dict) or any(
                dialect not in dialects or not isinstance(reason, str) or not reason
                for dialect, reason in pending.items()
            ):
                problems.append(f"`{scenario}` has invalid pending dialects: {pending}")
            if not isinstance(dialects, list) or not dialects:
                problems.append(f"`{scenario}` needs a nonempty dialect list")
            elif any(dialect not in declared for dialect in dialects) or len(
                set(dialects)
            ) != len(dialects):
                problems.append(f"`{scenario}` has undeclared or duplicate dialects: {dialects}")
    return problems


def discovery_violations(config: dict[str, object]) -> list[str]:
    """README-only harnesses also need an explicit inventory disposition."""
    classified = {
        name
        for group, policy in config["groups"].items()
        if policy.get("referent") != "harness_case"
        for name in config[group]
    }
    discovered = {path.name for path in (ROOT / "runbooks").glob("*/") if path.is_dir()}
    return [
        f"runbooks/{name}/ has no row or declared harness"
        for name in sorted(discovered - classified)
    ]


def tier_violations(config: dict[str, object]) -> list[str]:
    """Every scenario's tier and model, checked against the tier table.

    A model outside its tier's list is the failure this checks for: a row
    silently funded at a model its tier does not fund is exactly the mislabeled
    evidence the matrix exists to prevent, and it is invisible in a diff that
    only reads the tier word.
    """
    tiers = config["tiers"]
    problems = []
    for group in config["groups"]:
        if config["groups"][group].get("referent") == "harness":
            continue
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


def referent_violations(config: dict[str, object]) -> list[str]:
    """Every non-emitting row, checked against the thing it names.

    A runbook names its Markdown procedure, a harness names its directory,
    and a harness case names `runbooks/<harness>/cases/<case>`.
    """
    problems = []
    for group, policy in config["groups"].items():
        if policy["emits"]:
            continue
        for scenario in config[group]:
            if policy["referent"] == "runbook":
                if not (ROOT / "runbooks" / scenario / "runbook.md").is_file():
                    problems.append(
                        f"`{scenario}` is listed in `{group}` but "
                        f"runbooks/{scenario}/runbook.md does not exist"
                    )
            elif policy["referent"] == "harness":
                if not (ROOT / "runbooks" / scenario).is_dir():
                    problems.append(f"`{scenario}` is listed in `{group}` but its harness directory does not exist")
            elif policy["referent"] == "harness_case":
                for split in range(len(scenario) - 1, 0, -1):
                    if scenario[split] != "-":
                        continue
                    harness, case = scenario[:split], scenario[split + 1 :]
                    if (ROOT / "runbooks" / harness / "cases" / case).is_dir():
                        break
                else:
                    problems.append(
                        f"`{scenario}` is listed in `{group}` but names no "
                        f"`runbooks/<harness>/cases/<case>` directory"
                    )
    return problems


def deterministic_provider_violations(config: dict[str, object]) -> list[str]:
    """Rows funded at a paid tier whose runbook drives the dev provider.

    `tier_violations` reads only the matrix, so it cannot see the mismatch that
    actually costs money: a runbook whose every phase is served by the in-process
    dev provider, funded at `economy` or `frontier`. The slug is then never
    requested, and the row's evidence claims a driver that produced none of it --
    the mislabeled evidence RULES.md's tier rules exist to prevent, in the
    direction the model-slug check cannot catch.

    The selector `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO` is the public, grep-able
    switch RULES.md requires a deterministic row to name. Naming it is not by
    itself a violation: a row may be genuinely mixed, with scripted surfaces
    beside real provider phases. That row declares `deterministic_phases`, which
    is exactly the claim being made. So the rule is: name the selector, then
    either be `deterministic` or say which phases are.
    """
    problems = []
    for group, policy in config["groups"].items():
        if not policy["emits"]:
            continue
        for scenario, entry in config[group].items():
            if entry.get("tier") == "deterministic":
                continue
            runbook = ROOT / "runbooks" / scenario / "runbook.md"
            if not runbook.is_file():
                continue
            if "AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO" not in runbook.read_text():
                continue
            if entry.get("deterministic_phases") is not None:
                continue
            problems.append(
                f"`{scenario}` is tier `{entry.get('tier')}` funded at "
                f"`{entry.get('model')}`, but its runbook drives the dev provider "
                f"and declares no `deterministic_phases`"
            )
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
    violations = dialect_violations(config) + discovery_violations(config)
    if violations:
        print(f"matrix violations: {'; '.join(violations)}", file=sys.stderr)
        return 2
    violations = tier_violations(config)
    if violations:
        print(f"tier violations: {'; '.join(violations)}", file=sys.stderr)
        return 2
    violations = deterministic_provider_violations(config)
    if violations:
        print(f"tier violations: {'; '.join(violations)}", file=sys.stderr)
        return 2
    violations = referent_violations(config)
    if violations:
        print(f"dangling matrix rows: {'; '.join(violations)}", file=sys.stderr)
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
                "schema": "lash.judged-runbook-shard.v4",
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
