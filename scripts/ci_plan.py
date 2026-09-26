#!/usr/bin/env python3
"""Classify changed paths and validate the aggregate CI conclusion.

This is the repository's one change classifier. `classify_path` maps a path to
what it can affect; CI's job plan (`classify`), the push gate (`gate-scope`)
and `scripts/dev-test.py` (`dev_test_scope`) are projections of it. Only the
Python standard library is used, so it runs before any toolchain.
"""

from __future__ import annotations

import argparse
import ast
from dataclasses import dataclass, replace
import enum
from functools import lru_cache
import json
import os
import re
from pathlib import Path, PurePosixPath
import shlex
import subprocess
import sys
import tomllib
from typing import Mapping


REPO_ROOT = Path(__file__).resolve().parents[1]

# The one binary the Cargo workspace partition still owns on a trusted event.
WORKBENCH_MANIFEST_DIR = "examples/agent-workbench"

# The package whose test binaries `Test Postgres store` runs.
POSTGRES_STORE_MANIFEST_DIR = "crates/lash-postgres-store"


FAMILIES = (
    "rust",
    "stores",
    "functional_e2e",
    "workers_e2e",
    "restate_suites",
    "feature_lanes",
    "workbench",
    "regress",
    "schema",
    "facade",
    "tooling",
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


def _optional_dependency_names(manifest: Mapping, include_dev: bool) -> set[str]:
    """The names of dependency entries Cargo compiles only when a feature asks."""

    names: set[str] = set()
    for table in _dependency_tables(manifest, include_dev):
        for name, spec in table.items():
            if isinstance(spec, Mapping) and spec.get("optional") is True:
                names.add(name)
    return names


def _resolve_features(
    manifest: Mapping, requested: set[str], include_dev: bool
) -> tuple[set[str], dict[str, set[str]]]:
    """Expand a feature request through one package's `[features]` table.

    Returns the optional dependency entries the request turns on, and the
    features it asks of each dependency. `dep:x` enables `x`; `x/feat` enables
    `x` and asks it for `feat`; `x?/feat` asks for `feat` only if something
    else enables `x`. An optional dependency no feature names with a `dep:`
    form keeps Cargo's implicit same-named feature.
    """

    table = manifest.get("features", {})
    if not isinstance(table, Mapping):
        table = {}
    named_explicitly = {
        entry[len("dep:") :]
        for entries in table.values()
        if isinstance(entries, list)
        for entry in entries
        if isinstance(entry, str) and entry.startswith("dep:")
    }
    implicit = _optional_dependency_names(manifest, include_dev) - named_explicitly

    enabled: set[str] = set()
    dependency_features: dict[str, set[str]] = {}
    seen: set[str] = set()
    pending = list(requested)
    while pending:
        feature = pending.pop()
        if feature in seen:
            continue
        seen.add(feature)
        if feature in implicit:
            enabled.add(feature)
        entries = table.get(feature)
        if not isinstance(entries, list):
            continue
        for entry in entries:
            if not isinstance(entry, str):
                continue
            if entry.startswith("dep:"):
                enabled.add(entry[len("dep:") :])
            elif "/" in entry:
                name, _, wanted = entry.partition("/")
                weak = name.endswith("?")
                name = name[:-1] if weak else name
                dependency_features.setdefault(name, set()).add(wanted)
                if not weak:
                    enabled.add(name)
            else:
                pending.append(entry)
    return enabled, dependency_features


def _requested_features(
    name: str, spec: Mapping, workspace_dependencies: Mapping
) -> set[str]:
    """The features one dependency entry asks of the package it points at."""

    features: set[str] = set()
    default = True
    entries = []
    if spec.get("workspace") is True:
        inherited = workspace_dependencies.get(name)
        if isinstance(inherited, Mapping):
            entries.append(inherited)
    entries.append(spec)
    for entry in entries:
        listed = entry.get("features")
        if isinstance(listed, list):
            features.update(item for item in listed if isinstance(item, str))
        if "default-features" in entry:
            default = entry["default-features"] is not False
    if default:
        features.add("default")
    return features


@lru_cache(maxsize=None)
def workbench_dependency_dirs(repo_root: str | None = None) -> frozenset[str]:
    """The first-party manifest directories the workbench partition compiles.

    The untrusted Cargo workspace partition includes `agent-workbench` when a
    change reaches its transitive dependency closure or its dev-dependencies.
    The closure is read out of the workspace manifests rather
    than kept as a hand list: `scripts/test_ci_plan.py` cross-checks it against
    `cargo metadata` so the two can never drift apart.

    The walk is feature-aware, because Cargo's is: an optional dependency is
    compiled only when the feature set the workbench actually requests enables
    it. `lash-runtime` offers an optional module per host-wired extension
    (ADR 0079), and a crate behind a feature the workbench never turns on is
    not in the binary and cannot change what the job executes. Features
    accumulate per package and the walk re-expands on new ones, matching the
    way Cargo unifies features within one build.
    """

    return _first_party_closure(repo_root, WORKBENCH_MANIFEST_DIR, every_optional=False)


@lru_cache(maxsize=None)
def postgres_store_dependency_dirs(repo_root: str | None = None) -> frozenset[str]:
    """The first-party manifest directories the Postgres store suites compile.

    `Test Postgres store` runs the `lash-postgres-store` test binaries, and a
    change anywhere in their first-party closure can break them: FIG-3595
    and FIG-3550 were runtime changes outside every store crate. The walk
    takes every optional dependency, not only the ones a feature enables:
    trusted events build those binaries under Bazel at the workspace's
    unified feature resolution, which is a superset of what `cargo test -p`
    asks for. `scripts/test_ci_plan.py` cross-checks it against the declared
    graph in `cargo metadata`.
    """

    return _first_party_closure(repo_root, POSTGRES_STORE_MANIFEST_DIR, every_optional=True)


def _first_party_closure(
    repo_root: str | None, start: str, *, every_optional: bool
) -> frozenset[str]:
    """The first-party manifest directories `start`'s tests compile."""

    root = Path(repo_root) if repo_root is not None else REPO_ROOT

    def manifest(directory: str) -> Mapping:
        with (root / directory / "Cargo.toml").open("rb") as handle:
            return tomllib.load(handle)

    with (root / "Cargo.toml").open("rb") as handle:
        workspace_manifest = tomllib.load(handle)
    workspace_dependencies = workspace_manifest.get("workspace", {}).get("dependencies", {})

    # The start package's own dev-dependencies compile for its tests; a
    # transitive dependency's dev-dependencies do not, exactly as `cargo test
    # -p` resolves.
    requested: dict[str, set[str]] = {}
    visited: set[str] = set()
    pending = [(start, frozenset({"default"}), True)]
    while pending:
        directory, features, include_dev = pending.pop()
        known = requested.setdefault(directory, set())
        if directory in visited and features <= known:
            continue
        known |= features
        visited.add(directory)
        package = manifest(directory)
        enabled, dependency_features = _resolve_features(package, known, include_dev)
        for table in _dependency_tables(package, include_dev):
            for name, spec in table.items():
                if not isinstance(spec, Mapping):
                    continue
                if (
                    not every_optional
                    and spec.get("optional") is True
                    and name not in enabled
                ):
                    continue
                dependency = _first_party_dependency_dir(
                    name, spec, directory, workspace_dependencies
                )
                if dependency is None:
                    continue
                wanted = _requested_features(
                    name, spec, workspace_dependencies
                ) | dependency_features.get(name, set())
                pending.append((dependency, frozenset(wanted), False))
    return frozenset(requested)

GATED_JOBS = {
    "check": "facade",
    "repo-gates": "tooling",
    "lashlang-git-consumer": "rust",
    "feature-lanes": "rust",
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

# `feature-lanes` compiles and lints every lane variant on every trusted
# event whose diff can move a Rust build, pull requests included: a lane
# break is caused by an API change anywhere upstream, which the diff's own
# path set can never see. #1979 merged a variant that did not compile while
# the lane graph was dispatch-only, and #2285 broke the slack-clone live-E2E
# variant from a public engine API change while `feature-lanes` was still
# path-gated on the PR board. On a pull request the `feature_lanes` family
# output gates only the lane TEST steps (`_is_feature_gate_path` holds that
# rule); every other trusted event runs the whole lane board. The job needs
# the pool's cache credentials, so an untrusted pull request skips it
# entirely. `lashlang-git-consumer` stays dispatch-only, where the cut to the
# minimum PR board left it.
FEATURE_LANES_JOB = "feature-lanes"


# Jobs deferred entirely to the manual full-profile run (workflow_dispatch):
# their job-level conditions skip them on pull_request and merge_group events.
# There is no automatic trunk run to carry them any more — an automatic push to
# main triggers no CI at all — so a dispatch is their sole home, and it is the
# profile release.yml certifies against.
# postgres-store is intentionally absent: it is not dispatch-only but it is no
# longer pull-request work either. The merge group and the dispatch run every
# selected `lash-postgres-store` test binary while its simulator, pool-wait
# and cross-backend steps remain dispatch-only; a pull request runs the
# suite's input changes through the affected Bazel targets only.
DISPATCH_ONLY_JOBS = {
    "heavy-tests",
    "stack-budget",
    "s3-store",
    "functional-e2e",
    "functional-e2e-process-operations",
    "unicode-tests",
    "lashlang-git-consumer",
    # The fuzz smoke stays off the pull-request critical path by design: its
    # bounded corpus run guards trunk without taxing every PR (FIG-878).
    "fuzz-smoke",
}

DEFERRED_EVENTS = {"pull_request", "merge_group"}

# The PostgreSQL majors. One `postgres-store` job builds the store binaries
# once and runs every selected major against its own container, so a second
# major costs a container and a test run, not another runner and Bazel client.
# PG16 is the sole primary lane. PG14/PG18 compare catalog shape
# only; they are merge-group breadth when the diff touches a durable schema
# crate, and part of the full profile on workflow_dispatch (weekly/release
# certification). Weekly confidence backends remain the compatibility witness
# for unrelated landings.
POSTGRES_PRIMARY_LEG = {"postgres": "16", "role": "primary"}
POSTGRES_COMPATIBILITY_LEGS = [
    {"postgres": "14", "role": "compatibility"},
    {"postgres": "18", "role": "compatibility"},
]


def postgres_matrix(event_name: str, schema: bool = False) -> list[dict[str, str]]:
    if event_name == "workflow_dispatch" or (event_name == "merge_group" and schema):
        return [
            POSTGRES_COMPATIBILITY_LEGS[0],
            POSTGRES_PRIMARY_LEG,
            POSTGRES_COMPATIBILITY_LEGS[1],
        ]
    return [POSTGRES_PRIMARY_LEG]

UNGATED_JOBS = {
    "worker-artifacts",
    "plan",
    "lint",
    "hygiene",
    "restate-postgres-workers",
    "restate-postgres-workers-summary",
}

WORKERS_E2E_JOBS = {
    "worker-artifacts",
    "restate-postgres-workers",
    "restate-postgres-workers-summary",
}

# The functional-e2e legs that host the live Restate suites. They run on the
# full-profile dispatch only: a pull request no longer runs any E2E leg, so
# the set is a name for the matrix flags, not a conclusion exception.
RESTATE_SUITE_JOBS = frozenset(
    {"functional-e2e", "functional-e2e-process-operations"}
)

BAZEL_TEST_JOB = "bazel-tests"
# Trusted Rust events run the core partition in `bazel-tests`. The tail is
# breadth: merge groups and dispatches run it on the combined tree.
BAZEL_TEST_JOBS = frozenset({BAZEL_TEST_JOB, "bazel-tests-tail"})


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
    "confidence-mutation-authority": "full-consumer",
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
    if event_name not in {"schedule", "workflow_dispatch", "pull_request", "merge_group"}:
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


# The one path classifier.
#
# Three consumers ask the same question -- what can this touched path affect?
# -- and used to answer it with four tables that disagreed: this CI plan, the
# push gate's family scope, `scripts/dev-test.py`'s package selection, and the
# PR test selector. `classify_path` is now the only table. `classify` projects
# it onto CI job families, `gate_scope` onto the push gate's families, and
# `dev_test_scope` onto dev-test's package selection. A consumer may widen its
# own projection (dev-test runs its whole suite for a package manifest because
# it does not query reverse dependencies by default); it never re-reads paths.
class PathKind(enum.Enum):
    # Prose outside every package: affects nothing compiled or checked.
    DOCS = "docs"
    # Prose a Rust test reads at run time (`RUST_RUNTIME_DOC_INPUTS`).
    DOC_INPUT = "doc-input"
    # A path inside a first-party package directory (`PACKAGE_ROOTS`).
    PACKAGE = "package"
    # Test data outside every package: fixtures, published schemas, fuzz.
    DATA = "data"
    # Build and repository tooling that is not a shared input.
    TOOLING = "tooling"
    # CI machinery under `scripts/` or `.github/` whose consumers are known:
    # it selects exactly the families of the jobs that run or test it
    # (`ci_machinery_families`).
    CI = "ci"
    # A shared input: it can move the result of every job.
    SHARED = "shared"
    UNKNOWN = "unknown"


@dataclass(frozen=True)
class PathClass:
    kind: PathKind
    # `crates/<name>` for a PACKAGE path, else None.
    package: str | None = None
    # Whether the package directory carries a BUILD.bazel file.
    bazel_package: bool = False
    # A package's own Cargo.toml or BUILD.bazel.
    manifest: bool = False
    # The CI families a CI machinery path selects; empty for every other kind.
    families: frozenset[str] = frozenset()


PACKAGE_ROOTS = ("crates", "examples", "runbooks")
DATA_ROOTS = ("fixtures/", "schemas/", "fuzz/")
DOC_SUFFIXES = frozenset({".md", ".rst", ".txt"})

# Prose files that a Rust test reads at run time, so a change to one of them
# can fail the Rust suite without touching a single line of Rust.
# `test_ci_plan.py` sweeps every tracked `*.rs` for references to `docs/**`
# and `CONTEXT.md` and fails on any path not listed here, so a newly added
# runtime doc read reopens no hole silently.
RUST_RUNTIME_DOC_INPUTS = frozenset(
    {
        "docs/adr/0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md",
    }
)

# Root files that feed every job: the workspace manifest and lock, the
# toolchain pin, and the workspace-wide dependency policy.
SHARED_ROOT_FILES = frozenset({"Cargo.toml", "Cargo.lock", "justfile", "deny.toml"})
# Directories that feed every job: Cargo and nextest configuration.
SHARED_PREFIXES = (".cargo/", ".config/")
# Root build and repository tooling. `.bazelrc` sets flags for every Bazel
# action and `.gitleaksignore` feeds the hygiene job, but neither is a Cargo
# input, so they select the repository gates rather than every family.
TOOLING_ROOT_FILES = frozenset(
    {
        "BUILD.bazel",
        ".bazelrc",
        ".bazelversion",
        "clippy.toml",
        "rustfmt.toml",
        ".pre-commit-config.yaml",
        ".gitattributes",
        ".gitleaksignore",
    }
)
INERT_ROOT_FILES = frozenset({".gitignore"})


@lru_cache(maxsize=None)
def _is_bazel_package(root: str, package: str) -> bool:
    return (Path(root) / package / "BUILD.bazel").is_file()


# CI machinery: the scripts and GitHub configuration CI runs. A path here is
# not a global invalidator. It selects the families of the jobs that execute or
# test it, derived from the tree (`ci_machinery_families`), plus `tooling`,
# because the repository gates hold every script self-test.
CI_MACHINERY_PREFIXES = ("scripts/", ".github/")
CI_WORKFLOW = ".github/workflows/ci.yml"
CLASSIFIER = "scripts/ci_plan.py"
# The only CI machinery that re-runs everything: the file that defines the
# jobs. Every other workflow has its own triggers and maps to its own scope.
CI_GLOBAL_PATHS = frozenset({CI_WORKFLOW})
# CI machinery that no CI job, workflow, `justfile` recipe, other script or
# build input consumes. A person runs it by hand, or GitHub reads it. Its only
# CI proof is the repository gates. An entry needs a reason, and
# `test_ci_plan.py` fails when an entry gains a consumer or is removed.
UNCONSUMED_CI_PATHS: Mapping[str, str] = {
    ".github/actionlint.yaml": "actionlint finds it by name in `lint`, which runs on every event",
    ".github/dependabot.yml": "GitHub's Dependabot reads it; no CI job does",
    "scripts/ci_ensure_run.sh": "run by hand to recover a CI run GitHub dropped",
    "scripts/perf_baseline.py": "run by hand to compare two lash-perf ledgers",
    "scripts/profile_monty_comparison.sh": "run by hand to measure the TypeScript VM against Monty's published workloads",
    "scripts/tool-batch-baseline.sh": "run by hand for the tool-batch baseline measurement",
}

# Directories a CI script reads whole, one file per change -- the deferred-law
# shards, the replay-divergence shards. Name matching cannot see the glob, so
# each directory declares its reader here: every tracked file under the key is
# consumed by the value, which passes its own families on transitively.
SHARD_DIR_READERS: Mapping[str, str] = {
    "scripts/deferred-laws": "scripts/check_law_execution_receipts.py",
    "scripts/restate-divergences": "scripts/ci/restate_suite.py",
}

# The plan outputs a `ci.yml` job may read that are not families. A job's
# families are the family outputs it reads (`_job_families`); an output in
# neither set counts as every family. `postgres_compatibility` is the schema
# family's PG14/PG18 selection; `pr_tail_labels`, `pr_test_labels` and
# `pr_build_targets` are label lists, not gates.
PLAN_OUTPUT_FAMILIES: Mapping[str, frozenset[str]] = {
    **{family: frozenset({family}) for family in FAMILIES},
    "postgres_compatibility": frozenset({"schema"}),
    "bazel_trusted": frozenset(),
    "fail_open": frozenset(),
    "docs_only": frozenset(),
    "reason": frozenset(),
    "postgres_primary": frozenset(),
    "pr_tail_labels": frozenset(),
    "pr_test_labels": frozenset(),
    "pr_build_targets": frozenset(),
}
_PLAN_OUTPUT = re.compile(r"needs\.plan\.outputs\.([A-Za-z0-9_]+)")


def _job_families(text: str) -> frozenset[str]:
    """The families a `ci.yml` job runs for: the plan outputs it reads.

    A job that reads none runs on every event (`plan`, `lint`, `hygiene`, the
    aggregator), so a script only it runs is exercised whatever the plan says.
    """

    families: set[str] = set()
    for output in _PLAN_OUTPUT.findall(text):
        families |= PLAN_OUTPUT_FAMILIES.get(output, frozenset(FAMILIES))
    return frozenset(families)


# Tracked files outside `scripts/` and `.github/` that read CI machinery at
# build or test time: Bazel packages and rules, Cargo and nextest config, and
# Rust sources (`include_str!`, runfiles). What they read selects `rust`.
BUILD_READER_PATHSPECS = (
    "crates", "examples", "runbooks", "tools", "fuzz", ".cargo", ".config",
    "BUILD.bazel", "MODULE.bazel", ".bazelrc",
)

# Prose and data: a file of this kind names paths but never runs one. An npm
# `package.json` is the exception, because its `scripts` run commands.
PROSE_SUFFIXES = frozenset(DOC_SUFFIXES | {".json"})

_JOB_HEADER = re.compile(r"^  ([A-Za-z0-9_-]+):\s*(?:#.*)?$")
_ANCHOR = re.compile(r"^(\s*)(?:-\s+)?[\w-]*:?\s*&([\w-]+)\s*$")
_ALIAS = re.compile(r"\*([\w-]+)")
_TOKEN = re.compile(r"[\w.@-]+")
_IMPORT = re.compile(r"^\s*(?:from|import)\s+([\w.]+)", re.MULTILINE)
_ACTION_USE = re.compile(r"\.github/actions/([\w.-]+)")
_RECIPE_CALL = re.compile(r"(?:\bjust\s+|\brecipe:\s*)([A-Za-z_][\w-]*)")
_RECIPE_HEADER = re.compile(r"^@?([A-Za-z_][\w-]*)(?:\s[^:]*)?:(?!=)(.*)$")


def _strip_comments(text: str, python: bool = False) -> str:
    """Drop whole-line comments, and a Python file's docstrings: prose never
    runs what it names. A file that does not parse keeps its docstrings."""

    lines = text.splitlines()
    if python:
        try:
            tree = ast.parse(text)
        except (SyntaxError, ValueError):
            tree = None
        for node in ast.walk(tree) if tree is not None else ():
            body = getattr(node, "body", None)
            if not isinstance(body, list) or not body:
                continue
            first = body[0]
            if not (
                isinstance(first, ast.Expr)
                and isinstance(first.value, ast.Constant)
                and isinstance(first.value.value, str)
                and first.end_lineno is not None
            ):
                continue
            span = range(first.lineno - 1, first.end_lineno)
            opening = lines[span[0]].lstrip()
            closing = lines[span[-1]].rstrip()
            # Blank only a docstring that owns its lines outright.
            if opening[:1] in {'"', "'"} and closing[-1:] in {'"', "'"}:
                for index in span:
                    lines[index] = ""
    return "\n".join(
        line for line in lines if not line.lstrip().startswith(("#", "//"))
    )


def _workflow_jobs(text: str) -> dict[str, str]:
    """Each `ci.yml` job's text, with the YAML anchors it aliases inlined.

    A text split rather than a YAML parse, because this runs before any
    toolchain; `test_ci_plan.py` checks it against a real parse.
    """

    lines = text.splitlines()
    anchors: dict[str, str] = {}
    for index, line in enumerate(lines):
        match = _ANCHOR.match(line)
        if not match:
            continue
        indent = len(match.group(1))
        block = [line]
        for follower in lines[index + 1 :]:
            if follower.strip() and len(follower) - len(follower.lstrip()) <= indent:
                break
            block.append(follower)
        anchors[match.group(2)] = "\n".join(block)
    jobs: dict[str, list[str]] = {}
    current: list[str] | None = None
    in_jobs = False
    for line in lines:
        if not line.startswith(" ") and line.strip():
            in_jobs = line.startswith("jobs:")
            current = None
            continue
        header = _JOB_HEADER.match(line) if in_jobs else None
        if header:
            current = jobs.setdefault(header.group(1), [])
        elif current is not None:
            current.append(line)
    result = {}
    for job, body in jobs.items():
        text = "\n".join(body)
        aliased = [anchors[name] for name in _ALIAS.findall(text) if name in anchors]
        result[job] = _strip_comments("\n".join([text, *aliased]))
    return result


def _justfile_recipes(text: str) -> dict[str, str]:
    """Each recipe's text: its header (dependencies) and its body."""

    recipes: dict[str, list[str]] = {}
    current: list[str] | None = None
    for line in text.splitlines():
        if line[:1] in {" ", "\t"}:
            if current is not None:
                current.append(line)
            continue
        current = None
        if not line.strip() or line.startswith("#"):
            continue
        header = _RECIPE_HEADER.match(line)
        if header:
            dependencies = re.sub(r"\([^)]*\)", "", header.group(2))
            current = recipes.setdefault(
                header.group(1),
                [" ".join(f"just {name}" for name in _TOKEN.findall(dependencies))],
            )
    return {name: _strip_comments("\n".join(body)) for name, body in recipes.items()}


def _ci_machinery_node(path: str) -> str:
    """The unit a CI machinery path belongs to: a composite action's directory
    (its `action.yml` and licence) or the file itself."""

    parts = PurePosixPath(path).parts
    if len(parts) >= 3 and parts[:2] == (".github", "actions"):
        return "/".join(parts[:3])
    return path


def _git_lines(root: Path, *args: str) -> list[str]:
    output = _git(root, *args)
    return [entry for entry in output.split("\0") if entry]


@lru_cache(maxsize=None)
def ci_machinery_families(root: str | None = None) -> Mapping[str, frozenset[str] | None]:
    """The CI families each tracked CI machinery path selects.

    Read from the tree, not kept by hand. A consumer names what it runs: a
    `ci.yml` job (its steps, the anchors it aliases and the composite actions it
    uses), another workflow, a `justfile` recipe, another script, or a build
    input. A path selects the families of every consumer that names it, by file
    name, `.github/actions/<name>`, Python import or `just <recipe>`, closed
    transitively. Matching by name can only over-select, and that costs time,
    not a missed proof. Every path also selects `tooling`, because the
    repository gates hold the script self-tests.

    A value of None marks a path with no consumer that `UNCONSUMED_CI_PATHS`
    does not list, and `classify_path` fails that path open. `CI_GLOBAL_PATHS`
    is not in the map. Raises OSError, RuntimeError or ValueError when the tree
    cannot be read; `classify_path` then fails every CI machinery path open.
    """

    base = Path(root) if root is not None else REPO_ROOT
    tracked = _git_lines(base, "ls-files", "-z", "--", *CI_MACHINERY_PREFIXES)
    # `git grep` exits 1 when nothing matches, which is an answer, not a fault.
    grep = subprocess.run(
        ["git", "grep", "-l", "-z", "-I", "-E", r"scripts|\.github", "--",
         *BUILD_READER_PATHSPECS],
        cwd=base, capture_output=True, text=True, check=False,
    )
    if grep.returncode not in {0, 1}:
        raise RuntimeError(f"git grep failed ({grep.returncode}): {grep.stderr.strip()}")
    readers = [entry for entry in grep.stdout.split("\0") if entry]

    def read(path: str) -> str:
        return (base / path).read_text(encoding="utf-8", errors="replace")

    # Each consumer: (the node it is, or None for a root; its own families; text).
    consumers: list[tuple[str | None, frozenset[str], str]] = []
    for text in _workflow_jobs(read(CI_WORKFLOW)).values():
        consumers.append((None, _job_families(text), text))
    recipes = _justfile_recipes(read("justfile"))
    for name, text in recipes.items():
        consumers.append((f"justfile:{name}", frozenset(), text))
    for path in readers:
        name = PurePosixPath(path)
        if name.suffix in PROSE_SUFFIXES and name.name != "package.json":
            continue
        consumers.append(
            (None, frozenset({"rust"}), _strip_comments(read(path), path.endswith(".py")))
        )
    nodes: set[str] = set()
    for path in tracked:
        if path in CI_GLOBAL_PATHS:
            continue
        node = _ci_machinery_node(path)
        nodes.add(node)
        # This classifier names paths as data and runs none of them; read as a
        # consumer, its own tables would map every path they list.
        if PurePosixPath(path).suffix not in PROSE_SUFFIXES and path != CLASSIFIER:
            consumers.append(
                (node, frozenset(), _strip_comments(read(path), path.endswith(".py")))
            )

    # Index each node by the names a consumer uses for it.
    by_token: dict[str, set[str]] = {}
    by_module: dict[str, set[str]] = {}
    for node in nodes:
        name = PurePosixPath(node).name
        if node.startswith(".github/actions/"):
            continue
        by_token.setdefault(name, set()).add(node)
        if name.endswith(".py"):
            by_module.setdefault(name[: -len(".py")], set()).add(node)

    edges: dict[str | None, set[str]] = {}
    named: set[str] = set()
    families: dict[str, set[str]] = {node: set() for node in nodes}
    families.update({f"justfile:{name}": set() for name in recipes})
    for owner, own, text in consumers:
        targets: set[str] = set()
        for token in set(_TOKEN.findall(text)):
            targets |= by_token.get(token, set())
        for module in _IMPORT.findall(text):
            targets |= by_module.get(module.split(".")[0], set())
        for action in _ACTION_USE.findall(text):
            node = f".github/actions/{action}"
            if node in nodes:
                targets.add(node)
        for recipe in _RECIPE_CALL.findall(text):
            if recipe in recipes:
                targets.add(f"justfile:{recipe}")
        targets.discard(owner)
        named |= targets
        for target in targets:
            families[target] |= own
        if owner is not None:
            edges.setdefault(owner, set()).update(targets)

    # A shard directory's files are all read by its declared reader's glob.
    for directory, reader in SHARD_DIR_READERS.items():
        reader_node = _ci_machinery_node(reader)
        if reader_node not in nodes:
            continue
        for path in tracked:
            if path.startswith(directory + "/"):
                node = _ci_machinery_node(path)
                named.add(node)
                edges.setdefault(reader_node, set()).add(node)

    # Close transitively: a consumer passes on what its own consumers run it for.
    changed = True
    while changed:
        changed = False
        for owner, targets in edges.items():
            for target in targets:
                if not families[owner] <= families[target]:
                    families[target] |= families[owner]
                    changed = True

    result: dict[str, frozenset[str] | None] = {}
    for path in tracked:
        if path in CI_GLOBAL_PATHS:
            continue
        node = _ci_machinery_node(path)
        consumed = (
            node in named
            or path in UNCONSUMED_CI_PATHS
            # Every workflow is a root: GitHub runs it on its own triggers.
            or path.startswith(".github/workflows/")
        )
        result[path] = frozenset(families[node] | {"tooling"}) if consumed else None
    return result


def classify_path(path: str, root: Path | None = None) -> PathClass:
    """The one answer to "what can this touched path affect?"."""

    posix = PurePosixPath(path)
    parts = posix.parts
    name = posix.name
    suffix = posix.suffix.lower()
    if path in RUST_RUNTIME_DOC_INPUTS:
        return PathClass(PathKind.DOC_INPUT)
    if path in CI_GLOBAL_PATHS:
        return PathClass(PathKind.SHARED)
    if path.startswith(CI_MACHINERY_PREFIXES):
        try:
            families = ci_machinery_families(str(root or REPO_ROOT)).get(path)
        except (OSError, RuntimeError, ValueError):
            families = None
        if families is None:
            return PathClass(PathKind.UNKNOWN)
        return PathClass(PathKind.CI, families=families)
    if (
        path in SHARED_ROOT_FILES
        or path.startswith(SHARED_PREFIXES)
        or (len(parts) == 1 and name.startswith("rust-toolchain"))
    ):
        return PathClass(PathKind.SHARED)
    if (
        path in TOOLING_ROOT_FILES
        or path.startswith("tools/")
        or (len(parts) == 1 and name.startswith("MODULE.bazel"))
    ):
        return PathClass(PathKind.TOOLING)
    if parts and parts[0] in PACKAGE_ROOTS:
        if len(parts) < 3:
            # A file beside the packages, such as the judged runbook matrix.
            return PathClass(PathKind.TOOLING)
        package = "/".join(parts[:2])
        bazel = _is_bazel_package(str(root or REPO_ROOT), package)
        if suffix in DOC_SUFFIXES and not bazel:
            # Prose in a directory no build reads, such as a runbook.
            return PathClass(PathKind.DOCS)
        # Anything inside a package, markdown included, is package-local:
        # `include_str!` and test runfiles read package READMEs.
        return PathClass(
            PathKind.PACKAGE,
            package=package,
            bazel_package=bazel,
            manifest=name in {"Cargo.toml", "BUILD.bazel"},
        )
    if path.startswith(DATA_ROOTS):
        if suffix in DOC_SUFFIXES:
            return PathClass(PathKind.DOCS)
        return PathClass(PathKind.DATA)
    if (
        path.startswith("docs/")
        or suffix in DOC_SUFFIXES
        or name.startswith("LICENSE")
        or path in INERT_ROOT_FILES
    ):
        return PathClass(PathKind.DOCS)
    return PathClass(PathKind.UNKNOWN)


def _is_workbench_path(path: str) -> bool:
    return path.startswith(f"{WORKBENCH_MANIFEST_DIR}/")


def _is_workbench_dependency_path(path: str, workbench_dirs: frozenset[str]) -> bool:
    return any(path.startswith(f"{directory}/") for directory in workbench_dirs)


def _is_regress_path(path: str) -> bool:
    return path.startswith("crates/lash-regress/")


def _is_schema_path(path: str) -> bool:
    return path.startswith(("crates/lash-postgres-store/", "crates/lash-sqlite-store/"))


# `stores` gates `Test Postgres store`, whose pull-request and merge-group
# steps run every `lash-postgres-store` test binary against a live database.
# A package path selects it when the package is in those binaries'
# first-party closure (`postgres_store_dependency_dirs`): FIG-3595 and
# FIG-3550 were runtime changes outside every store crate, invisible to the
# hand list of store crates this replaced. The root `fixtures/` tree is their
# `//:durable_fixtures` input, and a SQL script or a SQLite database anywhere
# is store input. Build tooling does not select it, except the files the job
# itself runs its tests through (`POSTGRES_STORE_TOOLING`): the Lint job's
# workspace clippy compiles every one of these test targets, so a tooling diff
# that breaks their build fails there, and the release dispatch runs them all.
POSTGRES_STORE_TOOLING = frozenset(
    {
        "tools/bazel/postgres_slot_runner.sh",
        "tools/bazel/test_xml_runner.sh",
        "tools/bazel/junit_xml.py",
        "tools/bazel/postgres_test_labels.txt",
    }
)


def _is_stores_path(path: str, path_class: PathClass, store_dirs: frozenset[str]) -> bool:
    posix = PurePosixPath(path)
    if "migrations" in posix.parts or posix.suffix in {".sql", ".db"}:
        return True
    if path in POSTGRES_STORE_TOOLING:
        return True
    if path_class.kind is PathKind.PACKAGE:
        return path_class.package in store_dirs
    return path_class.kind is PathKind.DATA and path.startswith("fixtures/")


# `restate_suites` gates the live Restate board: the functional-e2e legs that
# run pinned `restate-server`s and the Restate + Postgres + S3 workers jobs.
# They ran on `workflow_dispatch` alone, so a pull request that broke the
# Restate execution path merged green and surfaced only in the next manual
# run — #2106 (FIG-3699) and #2148 (FIG-3697) both landed that way, and the
# workbench's Restate recovery coverage died with #2085.
#
# The selection is a path rule, not a dependency closure: the suite owners'
# first-party closures cover most of the workspace, so "whatever the suites
# link" would run the board on nearly every diff. The rule instead names the
# code the suites exercise, from three sources:
#
# * `restate_suite_dirs()` — the manifest directories of the test binaries
#   `scripts/restate-suites.toml` registers. The registry is the suite
#   inventory; deriving the owners from it keeps a new suite covered without
#   a second table.
# * `RESTATE_SUITE_PACKAGES` — what the registry cannot name: the Restate
#   endpoints and runbooks the suites mount, and `lash-conformance`, whose
#   law definitions the effect-group suite expands into its test binary.
# * `RESTATE_CORE_SUBTREES` — the Restate execution path inside the two
#   shared runtime crates, matched on path segments so `src/` and its
#   `tests/` kernel mirrors count alike.
RESTATE_SUITES_REGISTRY = "scripts/restate-suites.toml"


@lru_cache(maxsize=None)
def restate_suite_dirs(root: str | None = None) -> frozenset[str]:
    """The manifest directories owning the registered live Restate suites."""

    base = Path(root) if root is not None else REPO_ROOT
    with (base / RESTATE_SUITES_REGISTRY).open("rb") as handle:
        registry = tomllib.load(handle)
    return frozenset(
        PurePosixPath(suite["label"][2:].split(":", 1)[0]).as_posix()
        for suite in registry["suites"].values()
    )


# Packages whose whole tree a live Restate suite mounts or expands: the
# agent-service endpoint (`agent-service-restate-e2e` cargo-tests it beside a
# Restate container), the runbooks whose binaries and scenarios the workers
# and process-operations legs drive, and the conformance law catalogue the
# effect-group suite's `conformance_and_poison` cases are built from — a law
# edit can fail the suite without touching another selected path (#2148).
RESTATE_SUITE_PACKAGES = frozenset(
    {
        "crates/lash-conformance",
        "examples/agent-service",
        "runbooks/process-operations",
        "runbooks/restate-postgres-workers",
    }
)

# The Restate execution path inside the shared runtime crates: the subtrees
# #2106 and #2148 changed when they broke the live suites. A run of stems is
# matched consecutively, so `src/session/tool_execution`, its
# `tests/store_backed/kernel/...` mirror and a flat `tool_dispatch.rs` all
# count, while the crate's other machinery stays out.
RESTATE_CORE_SUBTREES: Mapping[str, tuple[tuple[str, ...], ...]] = {
    "crates/lash-core-execution": (
        ("runtime", "effect"),
        ("session", "tool_execution"),
        ("tool_dispatch",),
    ),
    "crates/lash-core": (
        ("turn_driver",),
        ("turn_loop",),
    ),
}


def _contains_stem_run(path: str, run: tuple[str, ...]) -> bool:
    stems = [PurePosixPath(part).stem for part in PurePosixPath(path).parts]
    width = len(run)
    return any(
        tuple(stems[index : index + width]) == run
        for index in range(len(stems) - width + 1)
    )


def _is_restate_suite_path(
    path: str, path_class: PathClass, suite_dirs: frozenset[str]
) -> bool:
    if path_class.kind is not PathKind.PACKAGE or path_class.package is None:
        return False
    if path_class.package in suite_dirs or path_class.package in RESTATE_SUITE_PACKAGES:
        return True
    subtrees = RESTATE_CORE_SUBTREES.get(path_class.package)
    if subtrees is None:
        return False
    # A shared crate's manifest can change the suites' build even though it
    # sits beside, not inside, the execution subtrees.
    return path_class.manifest or any(
        _contains_stem_run(path, subtree) for subtree in subtrees
    )


# `facade` gates the untrusted Cargo seal lane: only the facade crate's public
# API or the root manifests can break the API surface it seals. Trusted events
# seal on every run inside `bazel-tests`, where an unchanged seal is a cache
# hit.
def _is_facade_path(path: str) -> bool:
    return path.startswith("crates/lash/") or path in {"Cargo.toml", "Cargo.lock"}


# `tooling` gates repo-gates: shared inputs, build tooling and a package's own
# manifest are the only inputs its self-checks read (the feature-lane
# resolution and dependency-boundary checks read every package manifest).
def _is_tooling_class(path_class: PathClass) -> bool:
    return path_class.kind in {PathKind.SHARED, PathKind.TOOLING} or path_class.manifest


# `feature_lanes` gates the lane TEST steps of the `feature-lanes` job on a
# pull request: the job's compile and clippy run on every trusted Rust diff
# because a lane break is caused by an API change anywhere upstream, while the
# tests keep a path gate. Merge groups and dispatches run the whole lane
# board. The rule is deliberately narrower than lane membership -- the
# generated lanes cover nearly every package directory, so "the diff touched
# a lane package" would run the tests on almost every diff. A path is
# feature-relevant when it is:
#
# * the lane spec itself (`tools/bazel/feature_lanes.bzl`), the coverage
#   registry (`scripts/feature-coverage.toml`), or Bazel machinery the lanes
#   resolve through (`tools/bazel/`, the module and root build files);
# * a lane-covered package's own manifest, where its `[features]` table and
#   `dep:` entries live;
# * any other file inside a lane-covered package that carries a
#   `cfg(feature = ...)`-style predicate -- the definition of feature-gated
#   source. A file that cannot be read (a deletion of gated code is a
#   feature change) is conservatively feature-relevant;
# * a runtime data or doc input, because a lane test may read it.
#
# Anything else -- a file in a package no lane compiles, or a lane-covered
# package's ungated source -- cannot move a lane build or test.
FEATURE_LANES_SPEC = "tools/bazel/feature_lanes.bzl"
FEATURE_COVERAGE_PLAN = "scripts/feature-coverage.toml"

# The Bazel configuration that feeds lane resolution: the generator, the lane
# spec, the module graph and the workspace-wide Bazel inputs. Other tooling
# (kiln, pre-commit config) cannot move a lane compile.
_FEATURE_LANE_TOOLING_PREFIX = "tools/bazel/"
_FEATURE_LANE_TOOLING_FILES = frozenset(
    {"BUILD.bazel", ".bazelrc", ".bazelversion", "clippy.toml"}
)


@lru_cache(maxsize=None)
def feature_lane_package_dirs(root: str | None = None) -> frozenset[str]:
    """The package directories the generated feature lanes compile or test.

    The `FEATURE_LANE_*` tables in the generated lane spec name every label
    the lanes build, test or lint; their package prefixes are the directories
    a feature-lane resolution can reach. A diff outside every one of them
    cannot move a lane build.
    """

    def collect(value: object, labels: set[str]) -> None:
        if isinstance(value, str):
            labels.add(value)
        elif isinstance(value, (list, tuple)):
            for item in value:
                collect(item, labels)
        elif isinstance(value, dict):
            for key, item in value.items():
                collect(key, labels)
                collect(item, labels)

    base = Path(root) if root is not None else REPO_ROOT
    tree = ast.parse((base / FEATURE_LANES_SPEC).read_text(encoding="utf-8"))
    labels: set[str] = set()
    for node in tree.body:
        if (
            isinstance(node, ast.Assign)
            and isinstance(node.targets[0], ast.Name)
            and node.targets[0].id.startswith("FEATURE_LANE_")
        ):
            collect(ast.literal_eval(node.value), labels)
    return frozenset(
        label[2:].split(":", 1)[0] for label in labels if label.startswith("//")
    )


_CFG_HEAD = re.compile(r"\bcfg(?:_attr)?\s*\(")
_FEATURE_ATOM = re.compile(r"\bfeature\b")


def _declares_feature_cfg(text: str) -> bool:
    """Whether the source carries a `cfg`/`cfg_attr` predicate naming `feature`.

    `feature` must appear inside the attribute's parentheses: `target_feature`
    never counts, while `cfg(all(unix, feature = "x"))` does because the scan
    walks to the matching close paren rather than a fixed shape.
    """

    for match in _CFG_HEAD.finditer(text):
        depth = 1
        index = match.end()
        while index < len(text) and depth:
            if text[index] == "(":
                depth += 1
            elif text[index] == ")":
                depth -= 1
            index += 1
        if _FEATURE_ATOM.search(text, match.end(), index):
            return True
    return False


def _is_feature_gate_path(
    path: str, path_class: PathClass, lane_dirs: frozenset[str]
) -> bool:
    """Whether `path` can change what a feature lane compiles or runs."""

    if path == FEATURE_COVERAGE_PLAN:
        return True
    if path_class.kind is PathKind.TOOLING:
        return path.startswith(_FEATURE_LANE_TOOLING_PREFIX) or path.startswith(
            "MODULE.bazel"
        ) or path in _FEATURE_LANE_TOOLING_FILES
    if path_class.kind in {PathKind.DOC_INPUT, PathKind.DATA}:
        # A lane test may read it.
        return True
    if path_class.kind is not PathKind.PACKAGE or path_class.package not in lane_dirs:
        return False
    if path_class.manifest:
        # The [features] table and dep: entries live in the manifest.
        return True
    try:
        text = (REPO_ROOT / path).read_text(encoding="utf-8", errors="replace")
    except OSError:
        # A deleted or unreadable file: removing gated code is a feature change.
        return True
    return _declares_feature_cfg(text)


# The push gate's projection of `classify_path` (`scripts/push-gate.sh`).
# A family is only ever skipped when every touched path provably cannot affect
# it; a shared input, an unknown path, an empty path set or a git failure runs
# everything. Being wrong in the skip direction hides a real failure until CI;
# being wrong in the run direction costs wall-clock and nothing else.
class GateFamily(enum.Enum):
    """The push gate's families. Closed: `scoped` in push-gate.sh names one."""

    # fmt, check, clippy, the Rust suites and every source guard.
    RUST_COMPILE = "rust-compile"
    # The repository script self-test suite: CI's `tooling` family.
    SCRIPTS = "scripts"
    # The `.github/` guards; only a shared input can move them.
    WORKFLOWS = "workflows"

    @property
    def env_variable(self) -> str:
        """The shell variable `gate_family_runs` reads for this family."""
        return f"GATE_RUN_{self.name}"

    def __str__(self) -> str:
        return self.value


GATE_FAMILIES = tuple(GateFamily)
ALL_GATE_FAMILIES = frozenset(GateFamily)

_GATE_FAMILIES_BY_KIND = {
    PathKind.DOCS: frozenset(),
    PathKind.DOC_INPUT: frozenset({GateFamily.RUST_COMPILE}),
    PathKind.PACKAGE: frozenset({GateFamily.RUST_COMPILE}),
    PathKind.DATA: frozenset({GateFamily.RUST_COMPILE}),
    PathKind.TOOLING: frozenset({GateFamily.RUST_COMPILE, GateFamily.SCRIPTS}),
    PathKind.SHARED: ALL_GATE_FAMILIES,
    PathKind.UNKNOWN: ALL_GATE_FAMILIES,
}
# The CI families whose jobs compile or test Rust: all but the script gates.
COMPILE_FAMILIES = frozenset(FAMILIES) - {"tooling"}


def _gate_families(path_class: PathClass) -> frozenset[GateFamily]:
    if path_class.kind is not PathKind.CI:
        return _GATE_FAMILIES_BY_KIND[path_class.kind]
    # The script and workflow guards read CI machinery; the Rust battery runs
    # only when a job that compiles or tests Rust runs the path.
    families = {GateFamily.SCRIPTS, GateFamily.WORKFLOWS}
    if path_class.families & COMPILE_FAMILIES:
        families.add(GateFamily.RUST_COMPILE)
    return frozenset(families)


@dataclass(frozen=True)
class GateScope:
    """The push gate's verdict for one path set."""

    classification: str
    reason: str
    families: frozenset[GateFamily]

    def runs(self, family: GateFamily) -> bool:
        return family in self.families


def _sample(paths: list[str], limit: int = 3) -> str:
    shown = ", ".join(paths[:limit])
    remainder = len(paths) - limit
    return f"{shown}, +{remainder} more" if remainder > 0 else shown


def gate_scope(paths: list[str], root: Path | None = None) -> GateScope:
    """Map a touched-path list onto the push-gate families it can affect."""

    ordered = sorted(dict.fromkeys(path for path in paths if path.strip()))
    if not ordered:
        return GateScope(
            "empty-diff",
            "no touched paths were resolved; an empty path set is a resolution "
            "failure, not a proof of narrowness",
            ALL_GATE_FAMILIES,
        )
    buckets: dict[PathKind, list[str]] = {}
    families: set[GateFamily] = set()
    for path in ordered:
        path_class = classify_path(path, root)
        buckets.setdefault(path_class.kind, []).append(path)
        families |= _gate_families(path_class)
        if path_class.manifest:
            families.add(GateFamily.SCRIPTS)
    kinds = set(buckets)
    if PathKind.UNKNOWN in kinds:
        return GateScope(
            "unknown-paths",
            f"paths outside every known class: {_sample(buckets[PathKind.UNKNOWN])}",
            ALL_GATE_FAMILIES,
        )
    classification = (
        "shared-inputs" if PathKind.SHARED in kinds
        else "docs-only" if kinds == {PathKind.DOCS}
        else "rust-input-docs" if kinds == {PathKind.DOC_INPUT}
        else "rust-only" if kinds == {PathKind.PACKAGE}
        else "ci-machinery" if kinds == {PathKind.CI}
        else "mixed"
    )
    reason = "; ".join(
        f"{kind.value} ({_sample(buckets[kind])})"
        for kind in sorted(buckets, key=lambda kind: kind.value)
    )
    return GateScope(classification, reason, frozenset(families))


def render_gate_text(scope: GateScope) -> str:
    lines = [
        f"{family}: {'run' if scope.runs(family) else 'skip'}" for family in GATE_FAMILIES
    ]
    lines.append(f"classification: {scope.classification} -- {scope.reason}")
    return "\n".join(lines)


def render_gate_env(scope: GateScope) -> str:
    lines = [
        f"{family.env_variable}={1 if scope.runs(family) else 0}" for family in GATE_FAMILIES
    ]
    # The closed family set, so the consuming shell can refuse a name that is
    # not in it instead of reading an unset variable and running everything.
    lines.append(
        "GATE_SCOPE_FAMILIES=" + shlex.quote(" ".join(family.name for family in GATE_FAMILIES))
    )
    lines.append(f"GATE_SCOPE_CLASSIFICATION={shlex.quote(scope.classification)}")
    lines.append(f"GATE_SCOPE_REASON={shlex.quote(scope.reason)}")
    return "\n".join(lines)


def _git(repo: Path, *args: str) -> str:
    result = subprocess.run(
        ["git", *args], cwd=repo, capture_output=True, text=True, check=False
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"git {' '.join(args)} failed ({result.returncode}): {result.stderr.strip()}"
        )
    return result.stdout


def _worktree_paths(repo: Path) -> list[str]:
    """Uncommitted paths, so a dirty tree cannot be classified as narrow."""

    tokens = [token for token in _git(repo, "status", "--porcelain", "-z").split("\0") if token]
    paths: list[str] = []
    index = 0
    while index < len(tokens):
        entry = tokens[index]
        index += 1
        if len(entry) < 4:
            continue
        status, path = entry[:2], entry[3:]
        paths.append(path)
        # A rename or copy spends a second NUL-token on its source path, and
        # that path is touched too.
        if ("R" in status or "C" in status) and index < len(tokens):
            paths.append(tokens[index])
            index += 1
    return paths


def collect_gate_paths(repo: Path, base: str, head: str, worktree: bool) -> list[str]:
    merge_base = _git(repo, "merge-base", base, head).strip()
    if not merge_base:
        raise RuntimeError(f"no merge base between {base} and {head}")
    # `-z` so a path needing quoting arrives as itself rather than as an
    # escaped string that would classify as unknown.
    diff = _git(repo, "diff", "--name-only", "-z", f"{merge_base}..{head}")
    paths = [entry for entry in diff.split("\0") if entry]
    if worktree:
        paths.extend(_worktree_paths(repo))
    return paths


# `scripts/dev-test.py`'s projection of `classify_path`: which Bazel packages
# to test, whether to widen to the whole developer suite, whether to name the
# manual seal target, whether to run the repository gates, and which script
# self-tests are the direct proof of a script edit.
#
# Implementations whose direct proof is a named self-test rather than a
# test file of their own name.
SCRIPT_PROOFS = {
    "scripts/check-substrate-boundary.sh": "scripts/test_check_substrate_boundary.py",
    "scripts/dev-test.py": "scripts/test_dev_test.py",
    "scripts/ci_plan.py": "scripts/test_ci_plan.py",
    "scripts/drive-determinism-allowlist.count": "scripts/test_check_substrate_boundary.py",
    "scripts/drive-determinism-allowlist.txt": "scripts/test_check_substrate_boundary.py",
    "tools/bazel/test_batch_runner.sh": "scripts/test_test_batch_runner.py",
    "tools/bazel/junit_xml.py": "scripts/test_test_xml.py",
    "tools/bazel/test_xml_runner.sh": "scripts/test_test_xml.py",
}


@dataclass(frozen=True)
class DevTestScope:
    packages: tuple[str, ...]
    broad: bool
    facade: bool
    repository: bool
    script_tests: tuple[str, ...]


def dev_test_scope(
    paths: list[str], root: Path, script_tests: frozenset[str]
) -> DevTestScope:
    """Select dev-test's scope. `script_tests` is CI's script self-test inventory."""

    packages: set[str] = set()
    scripts: set[str] = set()
    broad = facade = repository = False
    for path in paths:
        path_class = classify_path(path, root)
        if path_class.kind is PathKind.DOCS:
            continue
        proof = SCRIPT_PROOFS.get(path, path)
        if proof in script_tests and PurePosixPath(proof).name.startswith("test_"):
            scripts.add(proof)
        elif path in {"Cargo.toml", "Cargo.lock"}:
            broad = facade = True
        elif path_class.kind is PathKind.PACKAGE:
            facade |= _is_facade_path(path)
            # Without `--dependents` dev-test does not query who depends on a
            # package, so a package manifest widens to the whole suite.
            if path_class.bazel_package and not path_class.manifest:
                packages.add("//" + path_class.package)
            else:
                broad = True
        elif path_class.kind in {PathKind.DOC_INPUT, PathKind.DATA}:
            broad = True
        elif path_class.kind is PathKind.CI:
            # The repository gates hold every script self-test; the developer
            # suite runs only when a Rust job runs the path.
            repository = True
            broad |= bool(path_class.families & COMPILE_FAMILIES)
        else:
            broad = repository = True
    return DevTestScope(
        tuple(sorted(packages)), broad, facade, repository, tuple(sorted(scripts))
    )


# The checked-in output of `tools/bazel/generate_build_files.py`: each Cargo
# package's manifest path and every generated label's kind and test-policy
# tags. The plan job runs before any toolchain, so the label -> package map is
# read out of this file rather than queried from Bazel.
TARGET_INVENTORY = "tools/bazel/target-inventory.json"
# The target kinds the generator counts as executable tests.
EXECUTABLE_TEST_KINDS = frozenset({"bin-unit-test", "test", "unit-test"})


@lru_cache(maxsize=None)
def _dev_deferred_labels(root: str | None = None) -> Mapping[str, tuple[str, ...]]:
    """Each package directory's `dev-deferred` test labels.

    `manual`, `pr-deferred` and `//examples/` labels stay out even if a policy
    edit ever combines the tags: a service-backed label proves nothing without
    its service, a pr-deferred suite is trunk work, and an examples leaf is in
    the tail for wall-clock reasons the PR leg deliberately sheds.
    """

    base = Path(root) if root is not None else REPO_ROOT
    inventory = json.loads((base / TARGET_INVENTORY).read_text(encoding="utf-8"))
    labels: dict[str, list[str]] = {}
    for package in inventory["packages"]:
        directory = PurePosixPath(package["manifest"]).parent.as_posix()
        for target in package["targets"]:
            label = target.get("label")
            tags = target.get("tags") or ()
            if (
                label is None
                or target.get("kind") not in EXECUTABLE_TEST_KINDS
                or "dev-deferred" not in tags
                or "manual" in tags
                or "pr-deferred" in tags
                or label.startswith("//examples/")
            ):
                continue
            labels.setdefault(directory, []).append(label)
    return {
        directory: tuple(sorted(members)) for directory, members in labels.items()
    }


def _all_dev_deferred_labels(root: str | None = None) -> list[str]:
    return sorted(
        label for labels in _dev_deferred_labels(root).values() for label in labels
    )


def pr_tail_labels(paths: list[str], root: Path | None = None) -> list[str]:
    """The sorted `dev-deferred` labels of every package a path touches.

    `bazel-tests-tail` runs only on merge groups and dispatches, and lash PRs
    are often admin-merged past the queue: a change under a package's
    directory otherwise lands without that package's deferred tests ever
    running, which is how #2109's `corpus_laws__test` expectations edit turned
    main red. The plan emits this list as `pr_tail_labels` and `bazel-tests`
    names it after its `-//:workspace_tail_tests` subtraction -- Bazel applies
    target patterns in order, so a positive label after the negative pattern
    re-adds it. Package ownership comes from `classify_path`, the one path
    table; adding a second table is how the old consumers drifted apart.
    """

    labels = _dev_deferred_labels(str(root) if root is not None else None)
    selected: set[str] = set()
    for path in paths:
        path_class = classify_path(path, root)
        if path_class.kind is PathKind.PACKAGE and path_class.package is not None:
            selected.update(labels.get(path_class.package, ()))
    return sorted(selected)


def dev_test_inventory(root: Path | None = None) -> tuple[set[str], dict[str, list[str]]]:
    """The generated Bazel dev-suite members and their test batches.

    `WORKSPACE_DEV_TEST_TARGETS` is every deterministic, service-free label
    `//:dev_tests` expands to; `WORKSPACE_TEST_BATCHES` folds a package's
    small tests into its one `:test_batch` action, so a package selection
    names the batch rather than its members.
    """

    wanted = {"WORKSPACE_DEV_TEST_TARGETS", "WORKSPACE_TEST_BATCHES"}
    values = {}
    base = root if root is not None else REPO_ROOT
    tree = ast.parse((base / "tools/bazel/workspace_targets.bzl").read_text())
    for node in tree.body:
        if isinstance(node, ast.Assign) and isinstance(node.targets[0], ast.Name):
            name = node.targets[0].id
            if name in wanted:
                values[name] = ast.literal_eval(node.value)
    return set(values["WORKSPACE_DEV_TEST_TARGETS"]), values["WORKSPACE_TEST_BATCHES"]


def batch_labels(members: set[str], batches: dict[str, list[str]]) -> list[str]:
    labels = set(members)
    for batch, children in batches.items():
        # Partial reverse-dependency selections must not widen to other members.
        if set(children) <= members:
            labels.difference_update(children)
            labels.add(batch)
    return sorted(labels)


def affected_bazel_labels(
    scope: DevTestScope,
    members: set[str],
    tail: list[str],
    batches: Mapping[str, list[str]],
) -> tuple[list[str], list[str]]:
    """The (`bazel test`, `bazel build`) label lists one dev-test scope selects.

    This is the single affected-target selection: `scripts/dev-test.py` runs
    it locally and the pull-request leg of `bazel-tests` runs it in CI.
    `members` is the dev-inventory labels the caller selected (the touched
    packages', or a reverse-dependency query's); `tail` is the touched
    packages' `dev-deferred` labels (`pr_tail_labels`) -- merge groups are the
    only events that run the tail job, so a pull request runs them itself, and
    the rule holds on a broad plan too: a package manifest widens the
    selection but is still a deferred test's input (#2109's corpus
    expectations file went red on main otherwise). A broad scope means the
    whole `//:dev_tests` suite; the facade seal rides a facade diff; a touched
    package no selected label covers gets a `:all` compile; and
    `//:schema_checks` rides whichever invocation is non-empty.
    """

    members = set(members) | set(tail)
    labels = ["//:dev_tests", *tail] if scope.broad else batch_labels(members, batches)
    if scope.facade:
        labels.append("//crates/lash:ui_fixtures")
    uncovered = [
        package
        for package in scope.packages
        if not any(label.split(":")[0] == package for label in members)
    ]
    # Service-only and compile-only packages still need compilation proof,
    # including mixed diffs that also select tests in another package.
    builds = [f"{package}:all" for package in uncovered] if uncovered and not scope.broad else []
    if labels:
        labels.append("//:schema_checks")
    if builds:
        builds.append("//:schema_checks")
    return labels, builds


def pr_affected_targets(
    paths: list[str],
    root: Path | None = None,
    broad: bool = False,
    tail: list[str] | None = None,
) -> tuple[list[str], list[str]]:
    """The (`bazel test`, `bazel build`) label lists a pull request runs.

    The same selection `scripts/dev-test.py` computes for the diff: the
    package members of `dev_test_scope`, the touched packages' deferred
    labels, and the shared assembly in `affected_bazel_labels`. The script
    self-test inventory does not participate -- CI runs those proofs in
    `repo-gates`, not in the Bazel partition. `broad` and `tail` let
    `classify` widen a run-everything diff to the whole dev suite and the
    whole deferred set.
    """

    scope = dev_test_scope(paths, root or REPO_ROOT, frozenset())
    if broad and not scope.broad:
        scope = replace(scope, broad=True)
    allowed, batches = dev_test_inventory(root)
    members = {label for label in allowed if label.split(":")[0] in scope.packages}
    if tail is None:
        tail = pr_tail_labels(paths, root)
    return affected_bazel_labels(scope, members, tail, batches)


def fail_open(reason: str) -> dict[str, str]:
    # An unclassifiable diff can touch any package, so its PR leg re-adds every
    # dev-deferred label. If the inventory itself cannot be read, the suite
    # label re-adds the whole tail, examples included.
    try:
        tail = " ".join(_all_dev_deferred_labels())
    except (OSError, ValueError, KeyError):
        tail = "//:workspace_tail_tests"
    outputs = {
        "docs_only": "false",
        "fail_open": "true",
        "reason": reason,
        "pr_tail_labels": tail,
        # The pull-request leg runs the whole fast suite plus the deferred
        # tail: what the tail job would run on a trusted event. The facade
        # seal target is a cheap no-op when it is not an input.
        "pr_test_labels": (
            f"//:dev_tests {tail} //crates/lash:ui_fixtures //:schema_checks"
        ),
        "pr_build_targets": "",
    }
    outputs.update({family: "true" for family in FAMILIES})
    return outputs


def classify(
    changes: list[tuple[str, str]],
    event_name: str = "",
    workbench_dirs: frozenset[str] | None = None,
    store_dirs: frozenset[str] | None = None,
    restate_dirs: frozenset[str] | None = None,
    lane_dirs: frozenset[str] | None = None,
) -> dict[str, str]:
    if not changes:
        raise PlanError("the changed path set was empty")
    if workbench_dirs is None:
        try:
            workbench_dirs = workbench_dependency_dirs()
        except (OSError, ValueError, KeyError, tomllib.TOMLDecodeError) as error:
            return fail_open(f"workbench dependency closure is underivable: {error}")
    if store_dirs is None:
        try:
            store_dirs = postgres_store_dependency_dirs()
        except (OSError, ValueError, KeyError, tomllib.TOMLDecodeError) as error:
            return fail_open(f"Postgres store dependency closure is underivable: {error}")
    if restate_dirs is None:
        try:
            restate_dirs = restate_suite_dirs()
        except (OSError, ValueError, KeyError, tomllib.TOMLDecodeError) as error:
            return fail_open(f"Restate suite registry is underivable: {error}")
    if lane_dirs is None:
        try:
            lane_dirs = feature_lane_package_dirs()
        except (OSError, ValueError, SyntaxError, KeyError) as error:
            return fail_open(f"feature lane inventory is underivable: {error}")
    unknown_statuses = sorted({status for status, _ in changes if status not in CHANGE_STATUSES})
    if unknown_statuses:
        statuses = ", ".join(repr(status) for status in unknown_statuses)
        return fail_open(f"unknown change statuses: {statuses}")

    paths = [path for _, path in changes]
    if any(not path or path.startswith("/") or "\x00" in path for path in paths):
        raise PlanError("the diff contained an invalid repository path")

    classes = {path: classify_path(path) for path in paths}
    kinds = {path: path_class.kind for path, path_class in classes.items()}
    global_invalidator = any(kind is PathKind.SHARED for kind in kinds.values())
    has_deletion = any(status == "D" for status, _ in changes)
    docs_deletion = any(
        status == "D" and kinds[path] is PathKind.DOCS for status, path in changes
    )
    ambiguous = sorted(path for path, kind in kinds.items() if kind is PathKind.UNKNOWN)
    docs_only = all(kind is PathKind.DOCS for kind in kinds.values()) and not has_deletion
    # CI machinery selects its own families; every other non-docs path is a
    # build input and turns on the Bazel partition plus its path families.
    ci_families = set().union(
        *(path_class.families for path_class in classes.values() if path_class.kind is PathKind.CI)
    )
    build = [
        path for path, kind in kinds.items() if kind not in {PathKind.DOCS, PathKind.CI}
    ]
    workbench_hit = any(
        _is_workbench_path(path) or _is_workbench_dependency_path(path, workbench_dirs)
        for path in build
    )
    only_workbench = bool(build) and all(_is_workbench_path(path) for path in build)
    only_ci = not build and bool(ci_families)
    run_everything = global_invalidator or bool(ambiguous) or docs_deletion

    outputs = {
        "docs_only": str(docs_only).lower(),
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
            else "ci-machinery diff"
            if only_ci
            else "workbench-only diff"
            if only_workbench and not ci_families
            else "production-relevant diff"
        ),
        # The dev-deferred labels of every package the diff touches. A
        # docs-only diff can name none; a diff that runs everything can touch
        # any package, so it re-adds the whole dev-deferred set.
        "pr_tail_labels": " ".join(pr_tail_labels(paths)),
        # The Bazel labels the pull-request leg of `bazel-tests` runs: the
        # affected-target selection `scripts/dev-test.py` computes for the
        # same diff. Empty on a docs-only diff, which runs no Bazel leg.
        "pr_test_labels": "",
        "pr_build_targets": "",
    }
    if docs_only:
        outputs.update({family: "false" for family in FAMILIES})
        outputs["workbench"] = "false"
        return outputs
    if run_everything:
        outputs.update({family: "true" for family in FAMILIES})
        # A run-everything diff can move any package, so its PR leg runs the
        # whole dev suite and re-adds every dev-deferred label -- the same
        # breadth `pr_tail_labels` reports for it.
        all_deferred = _all_dev_deferred_labels()
        outputs["pr_tail_labels"] = " ".join(all_deferred)
        try:
            pr_tests, pr_builds = pr_affected_targets(
                paths, broad=True, tail=all_deferred
            )
        except (OSError, ValueError, KeyError) as error:
            return fail_open(f"affected Bazel targets are underivable: {error}")
        outputs["pr_test_labels"] = " ".join(pr_tests)
        outputs["pr_build_targets"] = " ".join(pr_builds)
        return outputs
    # `rust` gates the Bazel partition, which owns every agent-workbench unit
    # case, including browser projection with a pinned Node interpreter. The
    # breadth families stay off a workbench-only diff: the workbench is an
    # example host, not a store or a worker.
    breadth = bool(build) and not only_workbench
    try:
        pr_tests, pr_builds = pr_affected_targets(paths)
    except (OSError, ValueError, KeyError) as error:
        return fail_open(f"affected Bazel targets are underivable: {error}")
    outputs["pr_test_labels"] = " ".join(pr_tests)
    outputs["pr_build_targets"] = " ".join(pr_builds)
    selected = {
        "rust": bool(build),
        "functional_e2e": breadth,
        "workers_e2e": breadth,
        "workbench": workbench_hit,
        "regress": any(_is_regress_path(path) for path in build),
        "restate_suites": any(
            _is_restate_suite_path(path, classes[path], restate_dirs)
            for path in build
        ),
        "feature_lanes": any(
            _is_feature_gate_path(path, classes[path], lane_dirs) for path in build
        ),
        "schema": any(_is_schema_path(path) for path in build),
        "facade": any(_is_facade_path(path) for path in build),
        "tooling": any(_is_tooling_class(classes[path]) for path in build),
        # `stores` follows the Postgres store closure (see _is_stores_path);
        # `functional_e2e` and `workers_e2e` keep the breadth flag because
        # their jobs are dispatch/label-only anyway. `restate_suites` stays a
        # real path rule even though a pull request no longer acts on it:
        # merge groups and the full profile still consume the selection.
        "stores": any(
            _is_stores_path(path, classes[path], store_dirs) for path in build
        ),
    }
    outputs.update(
        {family: str(selected[family] or family in ci_families).lower() for family in FAMILIES}
    )
    return outputs


def evaluate_conclusion(
    needs: Mapping[str, Mapping[str, object]],
    event_name: str = "",
    workers_e2e_enabled: bool | None = None,
    bazel_is_trusted: bool = True,
) -> list[str]:
    if workers_e2e_enabled is None:
        workers_e2e_enabled = True

    expected_jobs = UNGATED_JOBS | set(GATED_JOBS) | BAZEL_TEST_JOBS
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
            expectation = plan_outputs.get(family)
            if expectation not in {"true", "false"}:
                continue
            if expectation != "false":
                problems.append(f"plan.{family} is true for an exact docs-only diff")
    elif not any(plan_outputs.get(family) == "true" for family in FAMILIES):
        # A CI machinery diff may leave `rust` off, but it always selects
        # `tooling`: a non-docs plan that selects nothing is a classifier fault.
        problems.append("plan selects no family for a non-docs diff")

    # A pull request runs no E2E leg and no live-suite board: the whole set
    # is dispatch- or merge-group-only on the fast board, and a skipped
    # result is the expected one whatever the diff selected.
    for job in sorted(expected_jobs & set(needs)):
        result = needs[job].get("result")
        if job in BAZEL_TEST_JOBS:
            rust_on = plan_outputs.get("rust") == "true"
            wanted = (
                "success" if bazel_is_trusted and rust_on
                and (job == BAZEL_TEST_JOB or event_name != "pull_request")
                else "skipped"
            )
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if bazel_is_trusted else 'untrusted'}"
                    f"{'' if rust_on else ' non-rust'} event,"
                    f" expected {wanted}"
                )
            continue
        if job in WORKERS_E2E_JOBS and (
            event_name == "pull_request" or not workers_e2e_enabled
        ):
            if result != "skipped":
                problems.append(
                    f"workers E2E job {job} ended with {result!r} on a"
                    f" {event_name} event that does not run it, expected skipped"
                )
            continue
        if job in DISPATCH_ONLY_JOBS and event_name in DEFERRED_EVENTS:
            if result != "skipped":
                problems.append(
                    f"dispatch-only job {job} ended with {result!r} on a"
                    f" {event_name} event, expected skipped"
                )
            continue
        if job == FEATURE_LANES_JOB:
            rust_on = plan_outputs.get("rust") == "true"
            # The lanes compile on every trusted event whose diff can move a
            # Rust build, pull requests included. The `feature_lanes` output
            # gates only the job's lane-test steps, which a job-level result
            # cannot see.
            wanted = "success" if bazel_is_trusted and rust_on else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if bazel_is_trusted else 'untrusted'}"
                    f"{'' if rust_on else ' non-rust'} event,"
                    f" expected {wanted}"
                )
            continue
        if job == "check":
            # A trusted event seals the API inside `bazel-tests`; this Cargo
            # seal lane is the untrusted path, required when the facade moved.
            required = not bazel_is_trusted and plan_outputs.get("facade") == "true"
            wanted = "success" if required else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if bazel_is_trusted else 'untrusted'} event,"
                    f" expected {wanted}"
                )
            continue
        if job == "postgres-store" and event_name in DEFERRED_EVENTS:
            # The merge group keeps the store suite; a pull request covers a
            # store diff through its affected Bazel labels alone.
            wanted = (
                "success"
                if event_name == "merge_group"
                and plan_outputs.get("stores") == "true"
                else "skipped"
            )
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} on a {event_name} event, expected {wanted}"
                )
            continue
        if job == "workspace-tests":
            # On a trusted event the Bazel partition owns every deterministic
            # Rust binary -- the agent-workbench Node-gated case included --
            # with a pinned Node interpreter. An untrusted event has no Bazel
            # partition and keeps the full Cargo workspace run.
            required = not bazel_is_trusted and plan_outputs.get("rust") == "true"
            wanted = "success" if required else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if bazel_is_trusted else 'untrusted'} event,"
                    f" expected {wanted}"
                )
            continue
        if job == "unicode-tests" and event_name == "workflow_dispatch":
            wanted = (
                "success"
                if plan_outputs.get("regress") == "true"
                or fail_open_output == "true"
                else "skipped"
            )
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

    scope_parser = subparsers.add_parser(
        "gate-scope", help="classify a branch into the push-gate families it can affect"
    )
    scope_parser.add_argument("--base", default="origin/main")
    scope_parser.add_argument("--head", default="HEAD")
    scope_parser.add_argument(
        "--paths-from",
        help="read the touched paths from this file ('-' for stdin) instead of git",
    )
    scope_parser.add_argument("--format", choices=("text", "env"), default="text")
    scope_parser.add_argument(
        "--no-worktree", action="store_true", help="ignore uncommitted changes"
    )
    scope_parser.add_argument("--repo", type=Path, default=REPO_ROOT, help=argparse.SUPPRESS)

    subparsers.add_parser("conclusion")
    args = parser.parse_args()

    if args.command == "gate-scope":
        try:
            if args.paths_from:
                raw = (
                    sys.stdin.read()
                    if args.paths_from == "-"
                    else Path(args.paths_from).read_text(encoding="utf-8")
                )
                paths = [line.strip() for line in raw.splitlines()]
            else:
                paths = collect_gate_paths(
                    args.repo, args.base, args.head, not args.no_worktree
                )
        except (RuntimeError, OSError) as error:
            print(f"gate-scope: {error}", file=sys.stderr)
            print("gate-scope: callers must treat this as 'run everything'", file=sys.stderr)
            return 2
        scope = gate_scope(paths, args.repo)
        print(render_gate_env(scope) if args.format == "env" else render_gate_text(scope))
        return 0

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
        legs = postgres_matrix(args.event, args.schema == "true")
        _write_outputs(
            {
                "postgres_primary": " ".join(
                    leg["postgres"] for leg in legs if leg["role"] == "primary"
                ),
                "postgres_compatibility": " ".join(
                    leg["postgres"] for leg in legs if leg["role"] == "compatibility"
                ),
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
