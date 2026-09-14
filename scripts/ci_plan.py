#!/usr/bin/env python3
"""Classify CI changes and validate the aggregate CI conclusion."""

from __future__ import annotations

import argparse
from functools import lru_cache
import json
import os
from pathlib import Path, PurePosixPath
import sys
import tomllib
from typing import Mapping


REPO_ROOT = Path(__file__).resolve().parents[1]

# The one binary the Cargo workspace partition still owns on a trusted event.
WORKBENCH_MANIFEST_DIR = "examples/agent-workbench"


FAMILIES = (
    "rust",
    "stores",
    "functional_e2e",
    "workers_e2e",
    "workbench",
    "regress",
    "schema",
)
CHANGE_STATUSES = frozenset({"A", "M", "D", "T"})


def _dependency_tables(manifest: Mapping, include_dev: bool) -> list[Mapping]:
    """Every dependency table whose entries are compiled for this package."""

    kinds = ["dependencies", "build-dependencies"]
    if include_dev:
        kinds.append("dev-dependencies")
    tables = [manifest.get(kind, {}) for kind in kinds]
    for target in manifest.get("target", {}).values():
        tables.extend(target.get(kind, {}) for kind in kinds)
    return [table for table in tables if isinstance(table, Mapping)]


def _first_party_dependency_dir(
    name: str,
    spec: object,
    manifest_dir: str,
    workspace_dependencies: Mapping,
) -> str | None:
    """Resolve one dependency entry to its in-repo manifest directory, or None."""

    if not isinstance(spec, Mapping):
        return None
    if spec.get("workspace") is True:
        spec = workspace_dependencies.get(name, {})
        if not isinstance(spec, Mapping) or "path" not in spec:
            return None
        return PurePosixPath(spec["path"]).as_posix()
    path = spec.get("path")
    if not isinstance(path, str):
        return None
    return PurePosixPath(os.path.normpath(f"{manifest_dir}/{path}")).as_posix()


@lru_cache(maxsize=None)
def workbench_dependency_dirs(repo_root: str | None = None) -> frozenset[str]:
    """The first-party manifest directories the workbench partition compiles.

    The Cargo workspace partition builds `agent-workbench` and runs its tests,
    so a change to any workspace crate in that binary's transitive dependency
    closure — plus the workbench's own dev-dependencies — changes what the job
    would execute. The closure is read out of the workspace manifests rather
    than kept as a hand list: `scripts/test_ci_plan.py` cross-checks it against
    `cargo metadata` so the two can never drift apart.
    """

    root = Path(repo_root) if repo_root is not None else REPO_ROOT

    def manifest(directory: str) -> Mapping:
        with (root / directory / "Cargo.toml").open("rb") as handle:
            return tomllib.load(handle)

    with (root / "Cargo.toml").open("rb") as handle:
        workspace_manifest = tomllib.load(handle)
    workspace_dependencies = workspace_manifest.get("workspace", {}).get("dependencies", {})

    closure: set[str] = set()
    # The workbench's own dev-dependencies compile for its tests; a transitive
    # dependency's dev-dependencies do not, exactly as `cargo test -p` resolves.
    pending = [(WORKBENCH_MANIFEST_DIR, True)]
    while pending:
        directory, include_dev = pending.pop()
        if directory in closure:
            continue
        closure.add(directory)
        for table in _dependency_tables(manifest(directory), include_dev):
            for name, spec in table.items():
                dependency = _first_party_dependency_dir(
                    name, spec, directory, workspace_dependencies
                )
                if dependency is not None and dependency not in closure:
                    pending.append((dependency, False))
    return frozenset(closure)

GATED_JOBS = {
    "lashlang-git-consumer": "rust",
    "package-feature-checks": "rust",
    "runtime-feature-boundary": "rust",
    "workspace-tests": "rust",
    "heavy-tests": "rust",
    "stack-budget": "rust",
    "postgres-store": "stores",
    "s3-store": "stores",
    "functional-e2e": "functional_e2e",
    "functional-e2e-process-operations": "functional_e2e",
    "fuzz-smoke": "rust",
    "unused-deps": "rust",
    "unicode-tests": "regress",
}

# Dedicated compile configurations that witness a queued rust head. Each
# resolves its own feature graph -- named package features, `lash-runtime`
# without defaults, and lashlang consumed as an external Git dependency -- so
# none of them is covered by the workspace check. They stay skipped on
# pull_request, skip docs-only merge groups (workspace `test-doc` still
# compiles those heads), skip `push` (the queue already witnessed the SHA),
# and keep `rust` family behaviour on workflow_dispatch.
QUEUE_REQUIRED_COMPILE_JOBS = {
    "lashlang-git-consumer",
    "package-feature-checks",
    "runtime-feature-boundary",
}

# Jobs deferred entirely to trunk runs (push / workflow_dispatch): their
# job-level conditions skip them on pull_request and merge_group events.
# postgres-store is intentionally absent: its focused runtime Agent Scenario
# runs on pull requests and merge groups while its heavier steps remain trunk-only.
# The QUEUE_REQUIRED_COMPILE_JOBS above are absent for the same kind of reason:
# they are deferred on pull_request only, and required in the queue when rust
# ran.
TRUNK_ONLY_JOBS = {
    "heavy-tests",
    "stack-budget",
    "s3-store",
    "functional-e2e",
    "functional-e2e-process-operations",
    # The fuzz smoke stays off the pull-request critical path by design: its
    # bounded corpus run guards trunk without taxing every PR (FIG-878).
    "fuzz-smoke",
}

# The merge queue already validated these on the SHA that lands on main.
# Breadth jobs (heavy, S3, E2E, fuzz, stack-budget, unicode) keep running on
# push; this set does not. `workspace-tests` is deliberately absent: the Cargo
# partition is the workbench binary's only witness, and it runs on every trunk
# push so a break is attributed to the merge that caused it.
PUSH_SKIP_CORE_JOBS = {
    "facade-only-examples",
    "test-doc",
    "repo-gates",
    "unused-deps",
    "lashlang-git-consumer",
    "package-feature-checks",
    "runtime-feature-boundary",
    "bazel-tests",
    "lint",
    "postgres-store",
}

DEFERRED_EVENTS = {"pull_request", "merge_group"}

# The PostgreSQL matrix. PG16 is the sole primary lane on pull_request and
# merge_group. PG14/PG18 compare catalog shape only and run when the diff
# touches a durable schema crate, or on workflow_dispatch (the full profile,
# including weekly/release certification). Push skips the postgres job
# entirely (PUSH_SKIP_CORE_JOBS); weekly confidence backends remain the
# compatibility witness for unrelated landings.
POSTGRES_PRIMARY_LEG = {"postgres": "16", "role": "primary"}
POSTGRES_COMPATIBILITY_LEGS = [
    {"postgres": "14", "role": "compatibility"},
    {"postgres": "18", "role": "compatibility"},
]


def postgres_matrix(event_name: str, schema: bool = False) -> list[dict[str, str]]:
    if event_name == "pull_request" and not schema:
        return [POSTGRES_PRIMARY_LEG]
    if schema or event_name == "workflow_dispatch":
        return [
            POSTGRES_COMPATIBILITY_LEGS[0],
            POSTGRES_PRIMARY_LEG,
            POSTGRES_COMPATIBILITY_LEGS[1],
        ]
    return [POSTGRES_PRIMARY_LEG]

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

BAZEL_TEST_JOB = "bazel-tests"


# Confidence is a separate scheduled/manual workflow. Pin its producer and
# consumers here so its conclusion uses the same fail-closed entrypoint as CI.
CONFIDENCE_JOB_POLICY = {
    "confidence": "selector",
    "confidence-build": "full-producer",
    "confidence-harnesses": "full-consumer",
    "confidence-generated": "full-consumer",
    "confidence-minimizer": "full-consumer",
    "confidence-backends": "full-consumer",
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


def _is_workbench_path(path: str) -> bool:
    return path.startswith(f"{WORKBENCH_MANIFEST_DIR}/")


def _is_workbench_dependency_path(path: str, workbench_dirs: frozenset[str]) -> bool:
    return any(path.startswith(f"{directory}/") for directory in workbench_dirs)


def _is_regress_path(path: str) -> bool:
    return path.startswith("crates/lash-regress/")


def _is_schema_path(path: str) -> bool:
    return path.startswith(("crates/lash-postgres-store/", "crates/lash-sqlite-store/"))


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
        "fail_open": "true",
        "reason": reason,
    }
    outputs.update({family: "true" for family in FAMILIES})
    return outputs


def classify(
    changes: list[tuple[str, str]],
    event_name: str = "",
    workbench_dirs: frozenset[str] | None = None,
) -> dict[str, str]:
    if not changes:
        raise PlanError("the changed path set was empty")
    if workbench_dirs is None:
        try:
            workbench_dirs = workbench_dependency_dirs()
        except (OSError, ValueError, KeyError, tomllib.TOMLDecodeError) as error:
            return fail_open(f"workbench dependency closure is underivable: {error}")
    # Push is trunk: CI's `on:` restricts it to `main`, and the push run is the
    # only run that witnesses main's own tree. It always carries the workbench
    # partition, so a break that a queue run somehow missed still surfaces on
    # the branch it broke rather than on the next unrelated pull request.
    trunk_push = event_name == "push"
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
    docs_only = all(_is_docs_path(path) for path in paths) and not has_deletion
    non_docs = [path for path in paths if not _is_docs_path(path)]
    workbench_hit = trunk_push or any(
        _is_workbench_path(path) or _is_workbench_dependency_path(path, workbench_dirs)
        for path in paths
    )
    only_workbench = bool(non_docs) and all(_is_workbench_path(path) for path in non_docs)
    run_everything = global_invalidator or bool(ambiguous) or docs_deletion

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
            else "workbench-only diff"
            if only_workbench
            else "production-relevant diff"
        ),
    }
    if docs_only:
        outputs.update({family: "false" for family in FAMILIES})
        outputs["workbench"] = str(trunk_push).lower()
        return outputs
    if run_everything:
        outputs.update({family: "true" for family in FAMILIES})
        return outputs
    rust = not only_workbench
    outputs.update(
        {
            "rust": str(rust).lower(),
            "stores": str(rust).lower(),
            "functional_e2e": str(rust).lower(),
            "workers_e2e": str(rust).lower(),
            "workbench": str(workbench_hit).lower(),
            "regress": str(any(_is_regress_path(path) for path in paths)).lower(),
            "schema": str(any(_is_schema_path(path) for path in paths)).lower(),
        }
    )
    return outputs


def evaluate_conclusion(
    needs: Mapping[str, Mapping[str, object]],
    event_name: str = "",
    ref: str = "",
    workers_e2e_enabled: bool | None = None,
    bazel_is_trusted: bool = True,
) -> list[str]:
    if workers_e2e_enabled is None:
        workers_e2e_enabled = not (
            event_name == "push" and ref != "refs/heads/main"
        )

    expected_jobs = UNGATED_JOBS | set(GATED_JOBS) | {BAZEL_TEST_JOB}
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
    if fail_open_output == "true":
        for family in FAMILIES:
            if plan_outputs.get(family) not in {"true", None}:
                problems.append(
                    f"plan.{family} is {plan_outputs.get(family)!r} for a fail-open diff"
                )
    elif docs_only == "true":
        for family in FAMILIES:
            # A trunk push carries the workbench partition whatever it changed.
            if family == "workbench" and event_name == "push":
                continue
            expectation = plan_outputs.get(family)
            if expectation not in {"true", "false"}:
                continue
            if expectation != "false":
                problems.append(f"plan.{family} is true for an exact docs-only diff")
    elif (
        plan_outputs.get("rust") != "true"
        and plan_outputs.get("workbench") != "true"
    ):
        problems.append(
            "plan.rust and plan.workbench are both false for a non-docs diff"
        )

    for job in sorted(expected_jobs & set(needs)):
        result = needs[job].get("result")
        if event_name == "push" and job in PUSH_SKIP_CORE_JOBS:
            if result != "skipped":
                problems.append(
                    f"{job} ended with {result!r} on a push event, expected skipped"
                )
            continue
        if job == BAZEL_TEST_JOB:
            rust_on = plan_outputs.get("rust") == "true"
            wanted = "success" if bazel_is_trusted and rust_on else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if bazel_is_trusted else 'untrusted'}"
                    f"{'' if rust_on else ' non-rust'} event,"
                    f" expected {wanted}"
                )
            continue
        if job in WORKERS_E2E_JOBS and not workers_e2e_enabled:
            if result != "skipped":
                problems.append(
                    f"workers E2E job {job} ended with {result!r} while disabled, expected skipped"
                )
            continue
        if job in TRUNK_ONLY_JOBS and event_name in DEFERRED_EVENTS:
            if result != "skipped":
                problems.append(
                    f"trunk-only job {job} ended with {result!r} on a"
                    f" {event_name} event, expected skipped"
                )
            continue
        if job in QUEUE_REQUIRED_COMPILE_JOBS and event_name in DEFERRED_EVENTS:
            rust_on = plan_outputs.get("rust") == "true" or fail_open_output == "true"
            wanted = "success" if event_name == "merge_group" and rust_on else "skipped"
            if result != wanted:
                problems.append(
                    f"queue-required compile job {job} ended with {result!r} on a"
                    f" {event_name} event, expected {wanted}"
                )
            continue
        if job == "postgres-store" and event_name in DEFERRED_EVENTS:
            wanted = "success" if plan_outputs.get("stores") == "true" else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} on a {event_name} event, expected {wanted}"
                )
            continue
        if job == "workspace-tests":
            # On a trusted event the Bazel partition owns every deterministic
            # Rust binary, so the Cargo job runs only for the workbench
            # binary. An untrusted event has no Bazel partition and keeps the
            # full Cargo workspace run. A trunk push always runs it: main is
            # the tree the workbench binary has to stay green on.
            if event_name == "push":
                required = ref == "refs/heads/main"
            else:
                required = plan_outputs.get("workbench") == "true" or (
                    not bazel_is_trusted and plan_outputs.get("rust") == "true"
                )
            wanted = "success" if required else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if bazel_is_trusted else 'untrusted'} event,"
                    f" expected {wanted}"
                )
            continue
        if job == "unicode-tests":
            if event_name in {"push", "workflow_dispatch"}:
                wanted = "success"
            else:
                wanted = "success" if plan_outputs.get("regress") == "true" else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} on a {event_name} event, expected {wanted}"
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
    classify_parser.add_argument("--event", default="")

    fail_parser = subparsers.add_parser("fail-open")
    fail_parser.add_argument("--reason", required=True)

    matrix_parser = subparsers.add_parser("postgres-matrix")
    matrix_parser.add_argument("--event", required=True)
    matrix_parser.add_argument(
        "--schema",
        choices=("true", "false"),
        default="false",
    )

    subparsers.add_parser("conclusion")
    args = parser.parse_args()

    if args.command == "classify":
        try:
            outputs = classify(_read_nul_changes(args.paths_file), args.event)
        except (OSError, UnicodeError, PlanError) as error:
            outputs = fail_open(f"classification error: {error}")
        _write_outputs(outputs)
        return 0

    if args.command == "fail-open":
        _write_outputs(fail_open(args.reason))
        return 0

    if args.command == "postgres-matrix":
        _write_outputs(
            {
                "postgres_matrix": json.dumps(
                    postgres_matrix(args.event, args.schema == "true")
                )
            }
        )
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
    bazel_is_trusted = os.environ.get("BAZEL_TRUSTED")
    if bazel_is_trusted not in {"true", "false"}:
        print(
            f"Invalid BAZEL_TRUSTED: {bazel_is_trusted!r}, expected 'true' or 'false'",
            file=sys.stderr,
        )
        return 1
    problems = evaluate_conclusion(
        needs,
        os.environ.get("GITHUB_EVENT_NAME", ""),
        os.environ.get("GITHUB_REF", ""),
        workers_e2e_enabled == "true",
        bazel_is_trusted == "true",
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
