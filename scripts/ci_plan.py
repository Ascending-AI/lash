#!/usr/bin/env python3
"""Classify CI changes and validate the aggregate CI conclusion."""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path, PurePosixPath
import re
import sys
from typing import Mapping


FAMILIES = ("rust", "confidence", "stores", "functional_e2e", "workers_e2e")
CHANGE_STATUSES = frozenset({"A", "M", "D", "T"})

# Constants whose value decides Lashlang artifact identity: the semantic hash
# version feeds every `resource_operation:<hex>` id and the
# `tool-intent:v2:blake3:` digests derived from them, and the bytecode format
# version moves the compiled artifact those ids are taken over. Moving either
# invalidates literal pins that only execute under PostgreSQL 16 -- the runtime
# agent scenario, the cross-backend differential, and the postgres-store
# package suites -- none of which a PG14-only pull-request matrix can run. Three
# trunk outages in one night came from exactly that blind spot, so a diff that
# moves one of these widens the PostgreSQL matrix on the run that carries it.
IDENTITY_VERSION_CONSTANTS = (
    "LASHLANG_SEMANTIC_HASH_VERSION",
    "BYTECODE_FORMAT_VERSION",
)

# Matches an added or removed *definition* line for one of those constants, on
# either side of the diff. Keying on `const <NAME> ... =` rather than on the
# file path is deliberate: a constant that moves to another module still shows
# a removed definition line, and a bare `pub use ...::<NAME>;` re-export does
# not match, so the signal follows the definition wherever it lives.
IDENTITY_VERSION_DEFINITION = re.compile(
    r"^[+-](?!\+\+|--)"
    r".*\bconst\s+(?:" + "|".join(IDENTITY_VERSION_CONSTANTS) + r")\b[^=\n]*="
)

GATED_JOBS = {
    "facade-gates": "rust",
    "lashlang-git-consumer": "rust",
    "package-feature-checks": "rust",
    "runtime-feature-boundary": "rust",
    "workspace-tests": "rust",
    "heavy-tests": "rust",
    "stack-budget": "rust",
    "confidence-fast": "confidence",
    "confidence-fast-summary": "confidence",
    "postgres-store": "stores",
    "s3-store": "stores",
    "functional-e2e": "functional_e2e",
    "functional-e2e-process-operations": "functional_e2e",
    "fuzz-smoke": "rust",
    "unused-deps": "rust",
}

# The dedicated compile configurations that must witness every head the merge
# queue is about to publish. Each resolves its own feature graph -- named
# package features, `lash-runtime` without defaults, and lashlang consumed as an
# external Git dependency -- so none of them is covered by the workspace check.
# They are required on merge_group INCLUDING docs-only groups: the change
# classifier certifies a diff, not the health of the base the group is built
# over, and a docs-only group over an already broken base is exactly how
# #1199 and #1198 published a green conclusion over a head that did not
# compile. They stay skipped on pull_request and keep their `rust` family
# behaviour on push / workflow_dispatch (FIG-2854).
QUEUE_REQUIRED_COMPILE_JOBS = {
    "lashlang-git-consumer",
    "package-feature-checks",
    "runtime-feature-boundary",
}

# Jobs deferred entirely to trunk runs (push / workflow_dispatch): their
# job-level conditions skip them on pull_request and merge_group events per
# the 2026-08-25 CI-scope ruling; reassess after the FIG-2169 test-prune sweep.
# postgres-store is intentionally absent: its focused runtime Agent Scenario
# runs on pull requests and merge groups while its heavier steps remain trunk-only.
# The QUEUE_REQUIRED_COMPILE_JOBS above are absent for the same kind of reason:
# they are deferred on pull_request only, and required in the queue.
TRUNK_ONLY_JOBS = {
    "heavy-tests",
    "stack-budget",
    "confidence-fast",
    "confidence-fast-summary",
    "s3-store",
    "functional-e2e",
    "functional-e2e-process-operations",
    # The fuzz smoke stays off the pull-request critical path by design: its
    # bounded corpus run guards trunk without taxing every PR (FIG-878).
    "fuzz-smoke",
}

DEFERRED_EVENTS = {"pull_request", "merge_group"}

# Jobs that run only on the full profile (workflow_dispatch); every other
# event must show them skipped.
FULL_PROFILE_JOBS = {"facade-gates"}

UNGATED_JOBS = {
    "worker-artifacts",
    "plan",
    "facade-only-examples",
    "test-doc",
    "repo-gates",
    "lint",
    "diff-hygiene",
    "secret-scan",
    "restate-postgres-workers",
    "restate-postgres-workers-summary",
}

WORKERS_E2E_JOBS = {
    "worker-artifacts",
    "restate-postgres-workers",
    "restate-postgres-workers-summary",
}


# Confidence is a separate scheduled/manual workflow. Pin its producer and
# consumers here so its conclusion uses the same fail-closed entrypoint as CI.
CONFIDENCE_JOB_POLICY = {
    "confidence": "selector",
    "confidence-build": "full-producer",
    "confidence-harnesses": "full-consumer",
    "confidence-generated": "full-consumer",
    "confidence-minimizer": "full-consumer",
    "confidence-backends": "full-consumer",
    "confidence-workers": "full-consumer",
    "confidence-coverage": "full-consumer",
    "confidence-mutation-core": "full-consumer",
    "confidence-mutation-sim": "full-consumer",
    "confidence-mutation-packages": "full-consumer",
    "sim-search": "full-consumer",
}


def evaluate_confidence_conclusion(needs: Mapping, event_name: str, selector: str) -> list[str]:
    problems = []
    expected = set(CONFIDENCE_JOB_POLICY)
    for job in sorted(expected - set(needs)):
        problems.append(f"aggregator is missing needed job: {job}")
    for job in sorted(set(needs) - expected):
        problems.append(f"aggregator has unmapped needed job: {job}")
    active = event_name in {"schedule", "workflow_dispatch"}
    if event_name not in {"schedule", "workflow_dispatch", "push", "pull_request", "merge_group"}:
        problems.append(f"unknown Confidence event: {event_name!r}")
    if event_name == "schedule" and selector != "full":
        problems.append("scheduled Confidence must select full")
    if not selector:
        problems.append("Confidence selector is missing")
    for job in sorted(expected & set(needs)):
        policy = CONFIDENCE_JOB_POLICY[job]
        if policy not in {"selector", "full-producer", "full-consumer"}:
            problems.append(f"unknown Confidence policy for {job}: {policy!r}")
            continue
        required = active and ((selector != "full") if policy == "selector" else (selector == "full"))
        result = needs[job].get("result")
        wanted = "success" if required else "skipped"
        if result != wanted:
            problems.append(f"{job} ended with {result!r}, expected {wanted}")
    return problems


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
                ".md",
                ".rst",
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
        or path.startswith(("crates/", "examples/", "runbooks/", ".github/actions/", ".config/", "fuzz/"))
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
        "identity_versions": "true",
        "fail_open": "true",
        "reason": reason,
    }
    outputs.update({family: "true" for family in FAMILIES})
    return outputs


def detect_identity_version_change(diff_text: str) -> bool:
    """True when the exact diff adds or removes an identity-constant definition."""

    for line in diff_text.splitlines():
        if not line or line[0] not in "+-":
            continue
        if IDENTITY_VERSION_DEFINITION.match(line):
            return True
    return False


def classify(
    changes: list[tuple[str, str]], diff_text: str | None = None
) -> dict[str, str]:
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
    ambiguous = sorted(path for path in paths if not _is_known_path(path))
    # No diff content means no exact content signal, so widen rather than
    # guess: the expensive matrix is the safe side of this call.
    identity_versions = (
        True if diff_text is None else detect_identity_version_change(diff_text)
    )
    # An identity move is never a docs-only diff, whatever its paths say: an
    # ADR code fence quoting a definition line is enough to set the signal, and
    # `postgres-store` gates its own `if` on the stores family. Leaving the two
    # incoherent would widen the matrix for a job the same plan permits to
    # skip, and on a trunk push the conclusion would then demand a job that
    # never ran -- an unfixable red, which is the failure this gate exists to
    # prevent.
    docs_only = (
        all(_is_docs_path(path) for path in paths)
        and not has_deletion
        and not identity_versions
    )
    run_everything = global_invalidator or not docs_only or bool(ambiguous)

    outputs = {
        "rust_code": str(any(path.endswith(".rs") or path.startswith(("crates/", "src/", "tests/")) for path in paths)).lower(),
        "deps_config": str(any(PurePosixPath(path).name in {"Cargo.lock", "Cargo.toml"} or path.startswith((".cargo/", ".config/")) for path in paths)).lower(),
        "docs_only": str(docs_only).lower(),
        "workflows_only": str(all(path.startswith(".github/workflows/") for path in paths)).lower(),
        "e2e_relevant": str(any(path.startswith(("examples/", "runbooks/")) or "e2e" in PurePosixPath(path).parts for path in paths)).lower(),
        "scripts_gates": str(any(path.startswith("scripts/") or path in {"justfile", "deny.toml"} for path in paths)).lower(),
        "identity_versions": str(identity_versions).lower(),
        "fail_open": str(bool(ambiguous)).lower(),
        "reason": (
            "docs deletion"
            if docs_deletion
            else f"ambiguous paths: {', '.join(ambiguous)}"
            if ambiguous
            else "global invalidator"
            if global_invalidator
            else "Lashlang identity version moved"
            if identity_versions
            else "docs-only diff"
            if docs_only
            else "production-relevant diff"
        ),
    }
    outputs.update({family: str(run_everything).lower() for family in FAMILIES})
    return outputs


def evaluate_conclusion(
    needs: Mapping[str, Mapping[str, object]],
    event_name: str = "",
    ref: str = "",
    workers_e2e_enabled: bool | None = None,
) -> list[str]:
    if workers_e2e_enabled is None:
        workers_e2e_enabled = not (
            event_name == "push" and ref != "refs/heads/main"
        )

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
    identity_versions = plan_outputs.get("identity_versions")
    if docs_only not in {"true", "false"}:
        problems.append(f"plan output docs_only is {docs_only!r}, expected 'true' or 'false'")
    if fail_open_output not in {"true", "false"}:
        problems.append(f"plan output fail_open is {fail_open_output!r}, expected 'true' or 'false'")
    if identity_versions not in {"true", "false"}:
        problems.append(
            f"plan output identity_versions is {identity_versions!r}, expected 'true' or 'false'"
        )
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
        if job in WORKERS_E2E_JOBS and not workers_e2e_enabled:
            if result != "skipped":
                problems.append(
                    f"workers E2E job {job} ended with {result!r} while disabled, expected skipped"
                )
            continue
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
        # The dedicated compile lanes are judged by the event before the family
        # expectation below ever applies: a docs-only merge group still has to
        # show them green, because the plan classifies the diff and these jobs
        # witness the head (FIG-2854).
        if job in QUEUE_REQUIRED_COMPILE_JOBS and event_name in DEFERRED_EVENTS:
            wanted = "skipped" if event_name == "pull_request" else "success"
            if result != wanted:
                problems.append(
                    f"queue-required compile job {job} ended with {result!r} on a"
                    f" {event_name} event, expected {wanted}"
                )
            continue
        # A diff that moves a Lashlang identity version must not be able to
        # conclude green off a skipped or failed PostgreSQL job: the 14/16/18
        # matrix is where its literal pins execute, and it runs on this head in
        # this run -- never on a separate dispatch of some other commit.
        if job == "postgres-store" and (
            event_name in DEFERRED_EVENTS or identity_versions == "true"
        ):
            if result != "success":
                problems.append(
                    f"{job} ended with {result!r} while the diff moves a Lashlang "
                    "identity version, whose literal pins run only in this matrix, "
                    "expected success"
                    if identity_versions == "true"
                    else f"{job} ended with {result!r} on a {event_name} event, expected success"
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
    classify_parser.add_argument("--diff-file", type=Path)

    fail_parser = subparsers.add_parser("fail-open")
    fail_parser.add_argument("--reason", required=True)

    subparsers.add_parser("conclusion")
    args = parser.parse_args()

    if args.command == "classify":
        try:
            diff_text = (
                # Diffs carry binary hunks and paths that are not valid UTF-8;
                # a replaced byte cannot hide a `const NAME =` definition line,
                # while a decode error here would fail the whole classification
                # open and buy the expensive matrix for nothing.
                args.diff_file.read_bytes().decode("utf-8", errors="replace")
                if args.diff_file is not None
                else None
            )
            outputs = classify(_read_nul_changes(args.paths_file), diff_text)
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
    if os.environ.get("CONCLUSION_WORKFLOW") == "confidence":
        problems = evaluate_confidence_conclusion(
            needs, os.environ.get("GITHUB_EVENT_NAME", ""),
            os.environ.get("CONFIDENCE_SELECTOR", ""),
        )
        print(json.dumps(needs, indent=2, sort_keys=True))
        for problem in problems:
            print(f"Confidence conclusion rejected: {problem}", file=sys.stderr)
        if problems:
            return 1
        print("Confidence conclusion accepted: every stage satisfied its policy.")
        return 0
    workers_e2e_enabled = os.environ.get("WORKERS_E2E_ENABLED")
    if workers_e2e_enabled not in {"true", "false"}:
        print(
            f"Invalid WORKERS_E2E_ENABLED: {workers_e2e_enabled!r}, expected 'true' or 'false'",
            file=sys.stderr,
        )
        return 1
    problems = evaluate_conclusion(
        needs,
        os.environ.get("GITHUB_EVENT_NAME", ""),
        os.environ.get("GITHUB_REF", ""),
        workers_e2e_enabled == "true",
    )
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
