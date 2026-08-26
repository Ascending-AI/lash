#!/usr/bin/env python3
"""Classify CI changes and validate the aggregate CI conclusion."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import sys
from typing import Mapping


FAMILIES = ("rust", "confidence", "stores", "functional_e2e", "workers_e2e")
CHANGE_STATUSES = frozenset({"A", "M", "D", "T"})

GATED_JOBS = {
    "api-coverage": "rust",
    "semver-advisory": "rust",
    "lashlang-git-consumer": "rust",
    "package-feature-checks": "rust",
    "runtime-feature-boundary": "rust",
    "test-shard": "rust",
    "heavy-tests": "rust",
    "stack-budget": "rust",
    "confidence-fast": "confidence",
    "confidence-fast-summary": "confidence",
    "postgres-store": "stores",
    "s3-store": "stores",
    "functional-e2e": "functional_e2e",
}

# Jobs deferred to trunk runs (push / workflow_dispatch): their job-level
# conditions skip them on pull_request and merge_group events per the
# 2026-08-25 CI-scope ruling; reassess after the FIG-2169 test-prune sweep.
TRUNK_ONLY_JOBS = {
    "heavy-tests",
    "semver-advisory",
    "lashlang-git-consumer",
    "package-feature-checks",
    "runtime-feature-boundary",
    "stack-budget",
    "confidence-fast",
    "confidence-fast-summary",
    "postgres-store",
    "s3-store",
    "functional-e2e",
}

DEFERRED_EVENTS = {"pull_request", "merge_group"}

# Jobs that run only on the full profile (workflow_dispatch); every other
# event must show them skipped.
FULL_PROFILE_JOBS = {"api-coverage"}

UNGATED_JOBS = {
    "plan",
    "facade-only-examples",
    "test-doc",
    "repo-gates",
    "lint",
    "restate-postgres-workers",
    "restate-postgres-workers-summary",
}


class PlanError(ValueError):
    """Raised when a path set cannot be classified exactly."""


def _is_global_invalidator(path: str) -> bool:
    name = PurePosixPath(path).name
    return (
        name in {"Cargo.lock", "Cargo.toml"}
        or name.startswith("rust-toolchain")
        or path.startswith(".cargo/")
        or path == ".config/nextest.toml"
        or path.startswith(".github/workflows/")
        or path.startswith("scripts/")
        or path in {"justfile", "deny.toml"}
    )


def _is_docs_path(path: str) -> bool:
    name = PurePosixPath(path).name.lower()
    return (
        (
            path.startswith("docs/")
            and PurePosixPath(path).suffix.lower()
            in {
                ".css",
                ".html",
                ".ico",
                ".js",
                ".json",
                ".md",
                ".pagefind",
                ".pf_fragment",
                ".pf_index",
                ".pf_meta",
                ".png",
                ".rst",
                ".svg",
                ".txt",
            }
        )
        or (path.startswith("runbooks/") and PurePosixPath(path).suffix.lower() in {".md", ".rst", ".txt"})
        or (name.startswith("readme") and PurePosixPath(name).suffix in {"", ".md", ".rst", ".txt"})
        or name in {"contributing.md", "context.md", "security.md", "license", "license.md"}
    )


def _is_known_path(path: str) -> bool:
    suffix = PurePosixPath(path).suffix.lower()
    return (
        _is_global_invalidator(path)
        or _is_docs_path(path)
        or path.startswith(("crates/", "examples/", "runbooks/", ".github/actions/", ".config/"))
        or path.startswith(("src/", "tests/", "benches/"))
        or suffix in {".rs", ".toml", ".json", ".yaml", ".yml", ".lock"}
    )


def fail_open(reason: str) -> dict[str, str]:
    outputs = {
        "rust_code": "false",
        "deps_config": "false",
        "docs_only": "false",
        "workflows_only": "false",
        "e2e_relevant": "false",
        "scripts_gates": "false",
        "fail_open": "true",
        "reason": reason,
    }
    outputs.update({family: "true" for family in FAMILIES})
    return outputs


def classify(changes: list[tuple[str, str]]) -> dict[str, str]:
    if not changes:
        raise PlanError("the exact diff was empty")
    unknown_statuses = sorted({status for status, _ in changes if status not in CHANGE_STATUSES})
    if unknown_statuses:
        statuses = ", ".join(repr(status) for status in unknown_statuses)
        return fail_open(f"unknown change statuses: {statuses}")

    paths = [path for _, path in changes]
    if any(not path or path.startswith("/") or "\x00" in path for path in paths):
        raise PlanError("the diff contained an invalid repository path")

    global_invalidator = any(_is_global_invalidator(path) for path in paths)
    has_deletion = any(status == "D" for status, _ in changes)
    docs_deletion = any(status == "D" and _is_docs_path(path) for status, path in changes)
    docs_only = all(_is_docs_path(path) for path in paths) and not has_deletion
    ambiguous = sorted(path for path in paths if not _is_known_path(path))
    run_everything = global_invalidator or not docs_only or bool(ambiguous)

    outputs = {
        "rust_code": str(any(path.endswith(".rs") or path.startswith(("crates/", "src/", "tests/")) for path in paths)).lower(),
        "deps_config": str(any(PurePosixPath(path).name in {"Cargo.lock", "Cargo.toml"} or path.startswith((".cargo/", ".config/")) for path in paths)).lower(),
        "docs_only": str(docs_only).lower(),
        "workflows_only": str(all(path.startswith(".github/workflows/") for path in paths)).lower(),
        "e2e_relevant": str(any(path.startswith(("examples/", "runbooks/")) or "e2e" in PurePosixPath(path).parts for path in paths)).lower(),
        "scripts_gates": str(any(path.startswith("scripts/") or path in {"justfile", "deny.toml"} for path in paths)).lower(),
        "fail_open": str(bool(ambiguous)).lower(),
        "reason": (
            "docs deletion"
            if docs_deletion
            else f"ambiguous paths: {', '.join(ambiguous)}"
            if ambiguous
            else "global invalidator"
            if global_invalidator
            else "docs-only diff"
            if docs_only
            else "production-relevant diff"
        ),
    }
    outputs.update({family: str(run_everything).lower() for family in FAMILIES})
    return outputs


def evaluate_conclusion(
    needs: Mapping[str, Mapping[str, object]], event_name: str = ""
) -> list[str]:
    expected_jobs = UNGATED_JOBS | set(GATED_JOBS)
    problems: list[str] = []

    missing = sorted(expected_jobs - set(needs))
    unexpected = sorted(set(needs) - expected_jobs)
    if missing:
        problems.append(f"aggregator is missing needed jobs: {', '.join(missing)}")
    if unexpected:
        problems.append(f"aggregator has unmapped needed jobs: {', '.join(unexpected)}")

    plan = needs.get("plan", {})
    plan_outputs = plan.get("outputs", {})
    if not isinstance(plan_outputs, Mapping):
        plan_outputs = {}

    docs_only = plan_outputs.get("docs_only")
    fail_open_output = plan_outputs.get("fail_open")
    if docs_only not in {"true", "false"}:
        problems.append(f"plan output docs_only is {docs_only!r}, expected 'true' or 'false'")
    if fail_open_output not in {"true", "false"}:
        problems.append(f"plan output fail_open is {fail_open_output!r}, expected 'true' or 'false'")
    for family in FAMILIES:
        expectation = plan_outputs.get(family)
        required = docs_only != "true" or fail_open_output == "true"
        if expectation not in {"true", "false"}:
            continue
        if required and expectation != "true":
            problems.append(
                f"plan.{family} is false for a non-docs or fail-open diff; its skipped jobs are wrongly skipped"
            )
        elif not required and expectation != "false":
            problems.append(f"plan.{family} is true for an exact docs-only diff")

    for job in sorted(expected_jobs & set(needs)):
        result = needs[job].get("result")
        if job in FULL_PROFILE_JOBS and event_name != "workflow_dispatch":
            if result != "skipped":
                problems.append(
                    f"full-profile job {job} ended with {result!r} on a "
                    f"{event_name} event, expected skipped"
                )
            continue
        if job in TRUNK_ONLY_JOBS and event_name in DEFERRED_EVENTS:
            if result != "skipped":
                problems.append(
                    f"trunk-only job {job} ended with {result!r} on a"
                    f" {event_name} event, expected skipped"
                )
            continue
        if result in {"failure", "cancelled"}:
            problems.append(f"{job} ended with {result}")
            continue

        family = GATED_JOBS.get(job)
        if family is None:
            if result != "success":
                problems.append(f"ungated job {job} ended with {result!r}, expected success")
            continue

        expectation = plan_outputs.get(family)
        if expectation not in {"true", "false"}:
            problems.append(f"plan output {family} is {expectation!r}, expected 'true' or 'false'")
        elif expectation == "true" and result != "success":
            problems.append(f"{job} ended with {result!r} although plan.{family} required it to run")
        elif expectation == "false" and result not in {"success", "skipped"}:
            problems.append(f"{job} ended with {result!r} although plan.{family} allowed only success or skip")

    return problems


def _write_outputs(outputs: Mapping[str, str]) -> None:
    for key, value in outputs.items():
        if "\n" in value:
            raise PlanError(f"output {key} contains a newline")
        print(f"{key}={value}")


def _read_nul_changes(path: Path) -> list[tuple[str, str]]:
    raw = path.read_bytes()
    if raw and not raw.endswith(b"\0"):
        raise PlanError("the changed-path file was not NUL terminated")
    if not raw:
        return []

    fields = raw[:-1].split(b"\0")
    if len(fields) % 2:
        raise PlanError("the changed-path file did not contain status/path pairs")
    decoded = [field.decode("utf-8") for field in fields]
    return list(zip(decoded[::2], decoded[1::2], strict=True))


def main() -> int:
    parser = argparse.ArgumentParser()
    subparsers = parser.add_subparsers(dest="command", required=True)

    classify_parser = subparsers.add_parser("classify")
    classify_parser.add_argument("--paths-file", type=Path, required=True)

    fail_parser = subparsers.add_parser("fail-open")
    fail_parser.add_argument("--reason", required=True)

    subparsers.add_parser("conclusion")
    args = parser.parse_args()

    if args.command == "classify":
        try:
            outputs = classify(_read_nul_changes(args.paths_file))
        except (OSError, UnicodeError, PlanError) as error:
            outputs = fail_open(f"classification error: {error}")
        _write_outputs(outputs)
        return 0

    if args.command == "fail-open":
        _write_outputs(fail_open(args.reason))
        return 0

    try:
        needs = json.loads(os.environ["NEEDS_JSON"])
    except (KeyError, json.JSONDecodeError) as error:
        print(f"Invalid needs JSON: {error}", file=sys.stderr)
        return 1
    problems = evaluate_conclusion(needs, os.environ.get("GITHUB_EVENT_NAME", ""))
    print(json.dumps(needs, indent=2, sort_keys=True))
    if problems:
        print("CI conclusion rejected:", file=sys.stderr)
        for problem in problems:
            print(f"- {problem}", file=sys.stderr)
        return 1
    print("CI conclusion accepted: every job succeeded or was legitimately skipped.")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
