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
import fnmatch
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
    "pr_pg_store",
    "pr_host_restate",
    "functional_e2e",
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
    `cargo tree -p agent-workbench` so the two cannot drift apart.

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
    trusted events build those binaries under Buck2 at the workspace's
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
    "postgres-store-synthetic-next": "stores",
    "s3-store": "stores",
    "functional-e2e": "functional_e2e",
    "fuzz-smoke": "rust",
    "unused-deps": "rust",
    "unicode-tests": "regress",
}

# `feature-lanes` compiles and lints every lane variant on every trusted
# event whose diff can move a Rust build, pull requests included: a lane
# break is caused by an API change anywhere upstream, which the diff's own
# path set can never see. #1979 merged a variant that did not compile while
# the lane graph was dispatch-only, and #2285 broke the product live-E2E
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
# postgres-store is not dispatch-only. Selected store PRs run PG16 and the
# cross-backend differential; merge groups and dispatches retain their suites.
DISPATCH_ONLY_JOBS = {
    "heavy-tests",
    "stack-budget",
    "s3-store",
    "unicode-tests",
    "lashlang-git-consumer",
    # The fuzz smoke stays off the pull-request critical path by design: its
    # bounded corpus run guards trunk without taxing every PR (FIG-878).
    "fuzz-smoke",
}

DEFERRED_EVENTS = {"pull_request", "merge_group"}

# One job per build of the store package -- the default build and each feature
# variant -- so each has a runner and PostgreSQL slots of its own. They share
# one job condition, so the conclusion holds them to one rule.
POSTGRES_STORE_JOBS = ("postgres-store", "postgres-store-synthetic-next")

# The PostgreSQL majors. One `postgres-store` job builds the store binaries
# once and runs every selected major against its own container, so a second
# major costs a container and a test run, not another runner and Buck2 client.
# PG16 is the sole primary lane. PG14/PG18 compare catalog shape
# only; they are merge-group breadth when the diff touches a durable schema
# crate, and part of the full profile on workflow_dispatch (weekly/release
# certification).
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
    "plan",
    "lint",
    "hygiene",
}


STORE_PR_PACKAGES = frozenset(
    {
        "crates/lash",
        "crates/lash-lashlang-runtime",
        "crates/lash-plugin-process-controls",
        "crates/lash-protocol-rlm",
        "crates/lash-postgres-store",
        "crates/lash-sqlite-store",
        "crates/lash-core-store",
        "crates/lash-s3-store",
        "crates/lash-store-sql",
        "crates/lash-sim",
    }
)


def _is_pr_pg_store_path(path: str, path_class: PathClass) -> bool:
    return (
        path_class.package in STORE_PR_PACKAGES
        or "migrations" in PurePosixPath(path).parts
    )


def _is_pr_host_restate_path(path: str) -> bool:
    if path.startswith("examples/"):
        return True
    if path.startswith("crates/lash-core/src/runtime/"):
        return any(
            path.startswith(f"crates/lash-core/src/runtime/{part}")
            for part in ("shift/", "shift.rs", "turn_loop/", "turn_loop.rs")
        )
    return False

BUCK2_TEST_JOB = "buck2-tests"
# Trusted Rust events run the core partition in `buck2-tests`. The tail is
# breadth: merge groups and dispatches run it on the combined tree.
BUCK2_TEST_JOBS = frozenset({BUCK2_TEST_JOB, "buck2-tests-tail"})


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
    # Whether the package directory carries a BUCK file.
    buck2_package: bool = False
    # A package's own Cargo.toml or BUCK file.
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
        "crates/lash/docs/instrumentation-contract.md",
        "docs/adr/0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md",
    }
)

# Root files that feed every job: the workspace manifest and lock, the
# toolchain pin, and the workspace-wide dependency policy.
SHARED_ROOT_FILES = frozenset({"Cargo.toml", "Cargo.lock", "justfile", "deny.toml"})
# Directories that feed every job: Cargo and nextest configuration, plus the
# generated synthetic manifest, lock, source and Reindeer graph that resolve
# every Buck2 Rust dependency. Every `third-party/` path is build-affecting;
# prose there may be packaged crate data and is not classified as docs.
SHARED_PREFIXES = (".cargo/", ".config/", "third-party/")
# Root build and repository tooling. `.buckconfig` sets flags for every Buck2
# action and `.gitleaksignore` feeds the hygiene job, but neither is a Cargo
# input, so they select the repository gates rather than every family.
TOOLING_ROOT_FILES = frozenset(
    {
        "BUCK",
        ".buckconfig",
        "clippy.toml",
        "rustfmt.toml",
        ".pre-commit-config.yaml",
        ".gitattributes",
        ".gitleaksignore",
    }
)
INERT_ROOT_FILES = frozenset({".gitignore"})


@lru_cache(maxsize=None)
def _is_buck2_package(root: str, package: str) -> bool:
    return (Path(root) / package / "BUCK").is_file()


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
    "scripts/ci_ensure_run.sh": "run by hand to recover a CI run GitHub dropped",
    "scripts/perf_baseline.py": "run by hand to compare two lash-perf ledgers",
    "scripts/test_landing_gates.py": "run on the lander beside scripts/ci/landing-gates.sh; it needs `kiln` on PATH, which the repository-gates runners do not have",
    "scripts/generate-process-env-identity-golden.py": "run by hand to rewrite the process-environment identity fixture from the ignored generator's test log",
    "scripts/release-rehearsal.sh": "run by hand in a disposable Kiln fork through its private PostgreSQL gate to rehearse the 1.0 baseline reset",
    "scripts/e2e-workbench-operation.py": "its engine E2E caller went with Restate (FIG-5190); L9h (FIG-5186) rewires it",
    "scripts/e2e-workbench-recovery.py": "its engine E2E caller went with Restate (FIG-5190); L9h (FIG-5186) rewires it",
    "scripts/e2e-workbench-weather.py": "its engine E2E caller went with Restate (FIG-5190); L9h (FIG-5186) rewires it",
    "scripts/session_operator_e2e.py": "its engine E2E caller went with Restate (FIG-5190); L9h (FIG-5186) rewires it",
    "scripts/generate_loadtest_ledger.py": "its recipe built the deleted Restate workers runbook's ledger (FIG-5190); L9h (FIG-5186) rewires it",
}

# Directories a CI script reads whole, one file per change -- the
# replay-divergence shards. Name matching cannot see the glob, so each
# directory declares its reader here: every tracked file under the key is
# consumed by the value, which passes its own families on transitively.
SHARD_DIR_READERS: Mapping[str, str] = {}

# The plan outputs a `ci.yml` job may read that are not families. A job's
# families are the family outputs it reads (`_job_families`); an output in
# neither set counts as every family. `postgres_compatibility` is the schema
# family's PG14/PG18 selection; `pr_tail_labels`, `pr_test_labels` and
# `pr_build_targets` are label lists, not gates.
PLAN_OUTPUT_FAMILIES: Mapping[str, frozenset[str]] = {
    **{family: frozenset({family}) for family in FAMILIES},
    "postgres_compatibility": frozenset({"schema"}),
    "buck2_trusted": frozenset(),
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
# build or test time: Buck2 packages and rules, Cargo and nextest config, and
# Rust sources (`include_str!`, runfiles). What they read selects `rust`.
BUILD_READER_PATHSPECS = (
    "crates", "examples", "runbooks", "tools", "fuzz", ".cargo", ".config",
    "BUCK", ".buckconfig",
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
            if line.strip() and current is not None:
                current.append(line)
            continue
        # A blank line does not end a recipe body; the first dedented
        # non-blank line does.
        if not line.strip():
            continue
        current = None
        if line.startswith("#"):
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
        or (path.startswith("deploy/helm/") and suffix not in DOC_SUFFIXES)
    ):
        return PathClass(PathKind.TOOLING)
    if parts and parts[0] in PACKAGE_ROOTS:
        if len(parts) < 3:
            # A file beside the packages, such as the judged runbook matrix.
            return PathClass(PathKind.TOOLING)
        package = "/".join(parts[:2])
        buck2 = _is_buck2_package(str(root or REPO_ROOT), package)
        if suffix in DOC_SUFFIXES and not buck2:
            # Prose in a directory no build reads, such as a runbook.
            return PathClass(PathKind.DOCS)
        # Anything inside a package, markdown included, is package-local:
        # `include_str!` and test runfiles read package READMEs.
        return PathClass(
            PathKind.PACKAGE,
            package=package,
            buck2_package=buck2,
            manifest=name in {"Cargo.toml", "BUCK"},
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
# hand list of store crates this replaced. The root `fixtures/` tree holds the
# release fixtures their upgrade laws read, and a SQL script or a SQLite
# database anywhere is store input. Build tooling does not select it, except
# the files that choose, wrap or execute the live tests
# (`POSTGRES_STORE_TOOLING`). Workspace compilation cannot prove their slot,
# runtime-environment, timeout or result-reporting behavior, so their owning
# service suite must run.
POSTGRES_STORE_TOOLING = frozenset(
    {
        "scripts/hermetic-build.sh",
        "tools/buck2/driver.py",
        "tools/buck2/junit_xml.py",
        "tools/buck2/native-tools-lock.json",
        "tools/buck2/postgres_action_runner.py",
        "tools/buck2/postgres_slot_runner.sh",
        "tools/buck2/service_policy.py",
        "tools/buck2/target-inventory.json",
        "tools/buck2/test_launcher.sh",
        "tools/buck2/test_runner.py",
        "tools/buck2/test_shard.py",
        "tools/buck2/test_timeout.py",
        "tools/buck2/test_xml_runner.sh",
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


# A registered versioned surface is more than the file that defines its
# constant: the constant's `version_guard` marker names the shapes it guards,
# wherever they live. So the files of a surface are read from the markers, by
# the strict version-bump gate's own reader, and the gate (FIG-4494) selects on
# that one set.


@lru_cache(maxsize=None)
def versioned_surface_paths(root: str | None = None) -> frozenset[str]:
    """The path patterns of every registered versioned surface: the registry,
    each constant's file, each file a guard reads (globs included) and each
    migration catalog."""

    scripts = str(Path(__file__).resolve().parent)
    if scripts not in sys.path:
        sys.path.insert(0, scripts)
    import check_version_bumps

    try:
        return check_version_bumps.guarded_path_patterns(
            Path(root) if root is not None else REPO_ROOT
        )
    except check_version_bumps.CheckError as error:
        raise ValueError(str(error)) from error


def _is_versioned_surface_path(path: str, surface_paths: frozenset[str]) -> bool:
    return path in surface_paths or any(
        fnmatch.fnmatchcase(path, pattern) for pattern in surface_paths
    )


VERSION_BUMP_GATE_PATHS = frozenset(
    {"scripts/check_version_bumps.py", ".github/workflows/version-bumps.yml"}
)


def touches_versioned_surface(paths: list[str], root: Path | None = None) -> bool:
    """Whether any of `paths` is a registered surface's constant file, a file
    one of its guards reads, the registry, or the gate itself."""

    patterns = VERSION_BUMP_GATE_PATHS | versioned_surface_paths(
        None if root is None else str(root)
    )
    return any(_is_versioned_surface_path(path, patterns) for path in paths)


# `facade` gates the untrusted Cargo seal lane: only the facade crate's public
# API or the root manifests can break the API surface it seals. Trusted events
# seal on every run inside `buck2-tests`, where an unchanged seal is a cache
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
# * the target inventory, the coverage registry, or Buck2 machinery the lanes
#   resolve through;
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
FEATURE_LANES_SPEC = "tools/buck2/target-inventory.json"
FEATURE_COVERAGE_PLAN = "scripts/feature-coverage.toml"

# The Buck2 configuration that feeds lane resolution: the generator, the lane
# inventory and workspace-wide Buck2 inputs. Other tooling
# (kiln, pre-commit config) cannot move a lane compile.
_FEATURE_LANE_TOOLING_PREFIX = "tools/buck2/"
_FEATURE_LANE_TOOLING_FILES = frozenset(
    {"BUCK", ".buckconfig", "clippy.toml"}
)


@lru_cache(maxsize=None)
def feature_lane_package_dirs(root: str | None = None) -> frozenset[str]:
    """The package directories the generated feature lanes compile or test.

    The generated inventory names every lane unit. A diff outside every one
    of their packages cannot move a lane build.
    """

    base = Path(root) if root is not None else REPO_ROOT
    inventory = json.loads((base / FEATURE_LANES_SPEC).read_text(encoding="utf-8"))
    labels = {
        label
        for unit in inventory.get("feature_lane_units", ())
        for label in unit.values()
        if isinstance(label, str) and label.startswith("//")
    }
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
        return path.startswith(_FEATURE_LANE_TOOLING_PREFIX) or path in _FEATURE_LANE_TOOLING_FILES
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


# `scripts/dev-test.py`'s projection of `classify_path`: which Buck2 packages
# to test, whether to widen to the whole developer suite, whether to name the
# manual seal target, whether to run the repository gates, and which script
# self-tests are the direct proof of a script edit.
#
# Implementations whose direct proof is a named self-test rather than a
# test file of their own name.
SCRIPT_PROOFS = {
    "scripts/check-substrate-boundary.sh": "scripts/test_check_substrate_boundary.py",
    "scripts/check-substrate-port-ledger.py": "scripts/test_check_substrate_port_ledger.py",
    "scripts/check-dialect-boundary.py": "scripts/test_check_dialect_boundary.py",
    "scripts/check-history-readers.sh": "scripts/test_ci_plan.py",
    "scripts/check-guarded-transactions.py": "scripts/test_check_guarded_transactions.py",
    "scripts/guarded-transaction-readonly.txt": "scripts/test_check_guarded_transactions.py",
    "scripts/check-vm-static-state.py": "scripts/test_check_vm_static_state.py",
    "scripts/vm-static-state-allowlist.txt": "scripts/test_check_vm_static_state.py",
    "scripts/history-reader-allowlist.txt": "scripts/test_ci_plan.py",
    "scripts/history-reader-allowlist.count": "scripts/test_ci_plan.py",
    "scripts/ci/pg-service.sh": "scripts/test_pg_service.py",
    "scripts/dev-test.py": "scripts/test_dev_test.py",
    "scripts/ci_plan.py": "scripts/test_ci_plan.py",
    "scripts/shift-determinism-allowlist.count": "scripts/test_check_substrate_boundary.py",
    "scripts/shift-determinism-allowlist.txt": "scripts/test_check_substrate_boundary.py",
    "scripts/shift-store-allowlist.count": "scripts/test_check_substrate_boundary.py",
    "scripts/shift-store-allowlist.txt": "scripts/test_check_substrate_boundary.py",
    "tools/buck2/junit_xml.py": "scripts/test_test_xml.py",
    "tools/buck2/target-inventory.json": "scripts/test_ci_plan.py",
    "tools/buck2/test_shard.py": "scripts/test_buck2_test_contract.py",
    "tools/buck2/test_xml_runner.sh": "scripts/test_test_xml.py",
}


@dataclass(frozen=True)
class DevTestScope:
    packages: tuple[str, ...]
    broad: bool
    facade: bool
    repository: bool
    script_tests: tuple[str, ...]
    # The precise projection only (`dev_test_scope(..., base_text=...)`):
    # exact Buck2 labels and tracked files whose reverse dependencies the
    # change can move. A file stands for the targets that declare it as an
    # input (Buck2 `owner`). When Buck2 names no owner, a path in `files`
    # widens to the whole suite and a path in `machinery` selects nothing.
    targets: tuple[str, ...] = ()
    files: tuple[str, ...] = ()
    machinery: tuple[str, ...] = ()


# The inputs that select the whole developer suite even under the precise
# projection, with the reason no narrower selection is exact. Patterns are
# `fnmatch` globs over repository paths. Every other shared input has a rule in
# `_precise_selection`; a path with neither a rule nor a row here keeps the
# projection `dev_test_scope` gives it without a base.
DEV_TEST_GLOBAL_INPUTS: Mapping[str, str] = {
    "rust-toolchain*": "the compiler every action runs",
    ".buckconfig": "flags and cells of every Buck2 action",
    "tools/*": "Buck2 rules, toolchains, the generator, action sizes and test wrappers",
    "third-party/src/*": "the synthetic crate every third-party target resolves through",
    ".cargo/*": "Cargo configuration shared by every package",
    ".config/*": "nextest configuration shared by every package",
    "clippy.toml": "an input of every Rust target's lint action",
    "rustfmt.toml": "the format of every Rust source",
    "deny.toml": "the workspace-wide dependency policy",
    ".github/workflows/ci.yml": "the definition of every CI job",
    ".gitattributes": "checkout normalization of every file",
    ".pre-commit-config.yaml": "hooks over every file",
    ".gitleaksignore": "the hygiene scan over every file",
    "deploy/helm/*": "chart inputs with no declared reader",
}

ROOT_BUCK = "BUCK"
THIRD_PARTY_BUCK = "third-party/rust/BUCK"
# The generator's resolution inputs for `THIRD_PARTY_BUCK`. Buck2 reads the
# BUCK file, never these, so a change here moves a test only through the
# targets it regenerates, and those are diffed by target.
THIRD_PARTY_RESOLUTION = frozenset({"third-party/Cargo.toml", "third-party/Cargo.lock"})

_BUCK_STATEMENT = re.compile(r"[A-Za-z_][\w.]*\(")
_BUCK_NAME = re.compile(r'^\s+name = "([^"]+)",$', re.MULTILINE)


def buck_targets(text: str) -> tuple[dict[str, str], str] | None:
    """A generated BUCK file as ({target name: its rule call}, everything else).

    The generators write each rule call from a column-0 `rule(` line to a
    column-0 `)` line and name it on a `name = "..."` line. Everything outside
    a named call (loads, comments; blank lines dropped) is the second value.
    None when the text does not have that shape, so the caller selects the
    whole suite.
    """

    targets: dict[str, str] = {}
    rest: list[str] = []
    block: list[str] | None = None
    for line in text.splitlines():
        if block is not None:
            block.append(line)
            if line != ")":
                continue
            body = "\n".join(block)
            block = None
            match = _BUCK_NAME.search(body)
            if match is None:
                rest.append(body)
            elif match[1] in targets:
                return None
            else:
                targets[match[1]] = body
        elif _BUCK_STATEMENT.match(line) and line.count("(") > line.count(")"):
            block = [line]
        elif line[:1].isspace() or line == ")":
            # A continuation with no open call: not the generated shape.
            return None
        elif line:
            rest.append(line)
    return None if block is not None else (targets, "\n".join(rest))


def changed_buck_targets(old: str | None, new: str) -> frozenset[str] | None:
    """The targets of a generated BUCK file whose rule call was added or edited.

    A removed target is not listed: whatever depended on it names it in its
    own `deps`, so that rule call changed too. None when either side is not
    the generated shape or anything outside the rule calls differs.
    """

    before = buck_targets(old or "")
    after = buck_targets(new)
    if before is None or after is None or before[1] != after[1]:
        return None
    return frozenset(name for name, body in after[0].items() if before[0].get(name) != body)


def _lock_packages(text: str) -> dict[tuple[str, str, str | None], Mapping]:
    return {
        (package["name"], package["version"], package.get("source")): package
        for package in tomllib.loads(text).get("package", [])
    }


def changed_lock_packages(
    old: str | None, new: str
) -> frozenset[tuple[str, str, str | None]] | None:
    """The (name, version, source) of each `Cargo.lock` package whose entry
    was added or edited. A removed package is not listed: each package that
    depended on it has an edited `dependencies` list. None for an unreadable
    lock. `source` is None for a workspace member.
    """

    try:
        before = _lock_packages(old or "")
        after = _lock_packages(new)
    except (tomllib.TOMLDecodeError, KeyError, TypeError):
        return None
    return frozenset(key for key, package in after.items() if before.get(key) != package)


def changed_workspace_manifest(
    old: str | None, new: str
) -> tuple[frozenset[str], dict[str, tuple[object, object]]] | None:
    """What a root `Cargo.toml` edit changed, when that is only the member list
    or `[workspace.dependencies]` rows: (added members, {dependency key: (old
    row, new row)}). None when any shared section differs (`[workspace.package]`,
    lints, profiles, patches, the root package), which every crate inherits.
    A comment or layout edit changes nothing and returns two empty values.
    """

    try:
        before = tomllib.loads(old or "")
        after = tomllib.loads(new)
    except tomllib.TOMLDecodeError:
        return None
    before_workspace = dict(before.pop("workspace", {}))
    after_workspace = dict(after.pop("workspace", {}))
    before_members = before_workspace.pop("members", [])
    after_members = after_workspace.pop("members", [])
    before_rows = before_workspace.pop("dependencies", {})
    after_rows = after_workspace.pop("dependencies", {})
    if before != after or before_workspace != after_workspace:
        return None
    rows = {
        key: (before_rows.get(key), after_rows.get(key))
        for key in sorted(set(before_rows) | set(after_rows))
        if before_rows.get(key) != after_rows.get(key)
    }
    return frozenset(after_members) - frozenset(before_members), rows


def _worktree_text(root: Path, path: str) -> str:
    return (root / path).read_text(encoding="utf-8")


def _third_party_labels(root: Path, name: str, version: str | None = None) -> set[str]:
    """The third-party targets built from the crates.io archive of `name`
    (every locked version, or one). A locked package no target builds returns
    nothing: it cannot move a Buck2 test."""

    try:
        parsed = buck_targets(_worktree_text(root, THIRD_PARTY_BUCK))
    except OSError:
        parsed = None
    if parsed is None:
        raise ValueError(f"cannot read the targets of {THIRD_PARTY_BUCK}")
    locked = re.escape(version) if version else r'[0-9][^"]*'
    archive = re.compile(rf'":{re.escape(name)}-{locked}\.crate"')
    return {
        f"//third-party/rust:{target}"
        for target, body in parsed[0].items()
        if archive.search(body)
    }


@dataclass
class _Precise:
    packages: set[str]
    targets: set[str]
    files: set[str]
    scripts: set[str]
    machinery: set[str]
    broad: bool = False
    facade: bool = False
    repository: bool = False


def _precise_package(root: Path, directory: str, selection: _Precise) -> None:
    directory = PurePosixPath(directory).as_posix()
    if _is_buck2_package(str(root), directory):
        selection.packages.add("//" + directory)
    else:
        selection.broad = True


def _precise_selection(
    path: str,
    path_class: PathClass,
    root: Path,
    base_text,
    script_tests: frozenset[str],
) -> _Precise | None:
    """What one whole-suite trigger selects when its change can be read
    exactly, or None for a path with no precise rule.

    * A package's `BUCK` or `Cargo.toml` selects the package; the reverse
      dependency query adds what depends on it. A generated BUCK file changes
      with the manifest it is generated from, so both name the same package.
    * The root `BUCK` and `third-party/rust/BUCK` are diffed by target
      (`changed_buck_targets`); the synthetic third-party manifest and lock
      follow the BUCK file generated from them. All four keep the repository
      gates, which hold the generator's contracts.
    * `Cargo.lock` is diffed by package: a changed workspace member selects
      its package, a changed crates.io package the third-party targets built
      from that archive.
    * The root `Cargo.toml` is diffed by section (`changed_workspace_manifest`):
      a new member selects its package and a `[workspace.dependencies]` row the
      path package or every third-party target of that crate name.
    * A tracked data file (`schemas/`, `fixtures/`, `fuzz/`) or runtime doc
      input selects the targets that declare it as an input. The caller widens
      when Buck2 knows no owner, and `//:schema_checks` is built either way.
    * `justfile` selects the script self-tests that read it; no Buck2 target
      declares it as an input.
    * A source file in a package directory with no BUCK file (shared example
      sources) selects the targets that declare it, like a data file.
    * CI machinery (`scripts/`, `.github/`) keeps the repository gates and
      selects the targets that declare it as an input, if any. The Buck2 half
      of dev-test runs `kiln` (under `tools/`, a whole-suite input) and remote
      actions over declared inputs, so a script no target declares cannot
      change a dev-suite verdict: running the suite for it proved nothing
      about the CI job that runs the script.

    The lock and the root manifest are also Buck2 inputs of their own
    (`//:cargo_metadata`), so both are returned as files too.
    """

    selection = _Precise(set(), set(), set(), set(), set())
    try:
        if path_class.kind is PathKind.PACKAGE and path_class.manifest:
            assert path_class.package is not None
            selection.facade = _is_facade_path(path)
            _precise_package(root, path_class.package, selection)
        elif path in {ROOT_BUCK, THIRD_PARTY_BUCK}:
            # The generator's own contracts are repository gates.
            selection.repository = True
            package = PurePosixPath(path).parent.as_posix()
            changed = changed_buck_targets(
                base_text(path), _worktree_text(root, path)
            )
            if changed is None:
                selection.broad = True
            else:
                prefix = "//:" if package == "." else f"//{package}:"
                selection.targets.update(prefix + name for name in changed)
        elif path in THIRD_PARTY_RESOLUTION:
            selection.repository = True
        elif path == "Cargo.lock":
            selection.facade = True
            selection.files.add(path)
            changed_packages = changed_lock_packages(
                base_text(path), _worktree_text(root, path)
            )
            if changed_packages is None:
                selection.broad = True
                return selection
            members = _workspace_package_dirs(str(root))
            for name, version, source in sorted(
                changed_packages, key=lambda key: (key[0], key[1], key[2] or "")
            ):
                if source is not None:
                    selection.targets |= _third_party_labels(root, name, version)
                elif name in members:
                    _precise_package(root, members[name], selection)
                else:
                    selection.broad = True
        elif path == "Cargo.toml":
            selection.facade = True
            selection.files.add(path)
            changed_manifest = changed_workspace_manifest(
                base_text(path), _worktree_text(root, path)
            )
            if changed_manifest is None:
                selection.broad = True
                return selection
            members, rows = changed_manifest
            for member in sorted(members):
                _precise_package(root, member, selection)
            for key, sides in rows.items():
                for row in sides:
                    if row is None:
                        continue
                    table = row if isinstance(row, dict) else {}
                    if "path" in table:
                        _precise_package(root, table["path"], selection)
                    else:
                        selection.targets |= _third_party_labels(
                            root, table.get("package", key)
                        )
        elif path == "justfile":
            selection.scripts.update(_justfile_contract_tests(str(root), script_tests))
        elif path_class.kind is PathKind.CI:
            selection.repository = True
            if (root / path).is_file():
                selection.machinery.add(path)
        elif path_class.kind in {PathKind.DATA, PathKind.DOC_INPUT} or (
            path_class.kind is PathKind.PACKAGE and not path_class.buck2_package
        ):
            if (root / path).is_file():
                selection.files.add(path)
            else:
                # A deleted input changes a glob, which no file query can ask.
                selection.broad = True
        else:
            return None
    except (OSError, ValueError, KeyError):
        selection.broad = True
    return selection


@lru_cache(maxsize=None)
def _workspace_package_dirs(root: str) -> Mapping[str, str]:
    """Each workspace member's Cargo package name -> its directory."""

    inventory = json.loads((Path(root) / TARGET_INVENTORY).read_text(encoding="utf-8"))
    return {
        package["package"]: PurePosixPath(package["manifest"]).parent.as_posix()
        for package in inventory["packages"]
        if "package" in package
    }


@lru_cache(maxsize=None)
def _justfile_contract_tests(root: str, script_tests: frozenset[str]) -> frozenset[str]:
    """The script self-tests that read `justfile`: its recipes' contracts."""

    readers = set()
    for test in script_tests:
        if not PurePosixPath(test).name.startswith("test_"):
            continue
        try:
            text = (Path(root) / test).read_text(encoding="utf-8", errors="replace")
        except OSError:
            continue
        if "justfile" in text:
            readers.add(test)
    return frozenset(readers)


def _is_dev_test_global(path: str) -> bool:
    return any(fnmatch.fnmatchcase(path, pattern) for pattern in DEV_TEST_GLOBAL_INPUTS)


def dev_test_scope(
    paths: list[str], root: Path, script_tests: frozenset[str], base_text=None
) -> DevTestScope:
    """Select dev-test's scope. `script_tests` is CI's script self-test inventory.

    `base_text(path)` returns a path's text at the comparison base (None when
    it did not exist) and turns on the precise projection, which reads what a
    shared input's change is instead of widening to the whole suite. Only a
    caller that asks Buck2 for reverse dependencies may pass it: the precise
    rules name seeds, not every test they reach.
    """

    packages: set[str] = set()
    scripts: set[str] = set()
    targets: set[str] = set()
    files: set[str] = set()
    machinery: set[str] = set()
    broad = facade = repository = False
    for path in paths:
        path_class = classify_path(path, root)
        if path_class.kind is PathKind.DOCS:
            continue
        proof = SCRIPT_PROOFS.get(path, path)
        if proof in script_tests and PurePosixPath(proof).name.startswith("test_"):
            scripts.add(proof)
            continue
        if base_text is not None:
            if _is_dev_test_global(path):
                broad = repository = True
                continue
            precise = _precise_selection(path, path_class, root, base_text, script_tests)
            if precise is not None:
                packages |= precise.packages
                targets |= precise.targets
                files |= precise.files
                machinery |= precise.machinery
                scripts |= precise.scripts
                broad |= precise.broad
                facade |= precise.facade
                repository |= precise.repository
                continue
        if path in {"Cargo.toml", "Cargo.lock"}:
            broad = facade = True
        elif path_class.kind is PathKind.PACKAGE:
            facade |= _is_facade_path(path)
            # Without `--dependents` dev-test does not query who depends on a
            # package, so a package manifest widens to the whole suite.
            if path_class.buck2_package and not path_class.manifest:
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
        tuple(sorted(packages)), broad, facade, repository, tuple(sorted(scripts)),
        tuple(sorted(targets)), tuple(sorted(files)), tuple(sorted(machinery)),
    )


# Repository gates whose read set is closed, so the pre-land gate skips one
# when the change touches nothing it reads. The key is the command exactly as
# the gate inventory lists it. A gate with no row reads the tree in ways no
# table states (globs over scripts, the workflow, every manifest) and always
# runs. CI runs every gate regardless.
#
# `test_check_version_bumps.py` runs the version-bump gate over the working
# tree: its own modules, the surface registry, every Rust source and Cargo
# manifest under the package roots (`RUST_SOURCE_PATTERNS`,
# `CARGO_MANIFEST_PATTERNS`), and the files the registered guards name, all of
# which lie under the package roots (`test_ci_plan.py` holds that). It is the
# slowest gate by far: 260-385 s of pure-Python Rust parsing.
REPOSITORY_GATE_INPUTS: Mapping[str, tuple[str, ...]] = {
    "python3 scripts/test_check_version_bumps.py": (
        "scripts/test_check_version_bumps.py",
        "scripts/check_version_bumps.py",
        "scripts/discover_version_surfaces.py",
        "scripts/release_baseline.py",
        "scripts/versioned-surfaces.toml",
        "Cargo.toml",
        "crates/*",
        "examples/*",
        "runbooks/*",
    ),
}


def unaffected_repository_gates(paths: list[str]) -> tuple[str, ...]:
    """The repository gate commands no path in `paths` is an input of."""

    return tuple(
        command
        for command, patterns in REPOSITORY_GATE_INPUTS.items()
        if not any(
            fnmatch.fnmatchcase(path, pattern) for path in paths for pattern in patterns
        )
    )


# The checked-in output of `tools/buck2/generate.py`: each Cargo
# package's manifest path and every generated label's kind and test-policy
# tags. The plan job runs before any toolchain, so the label -> package map is
# read out of this file rather than queried from Buck2.
TARGET_INVENTORY = "tools/buck2/target-inventory.json"
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

    `buck2-tests-tail` runs only on merge groups and dispatches, and lash PRs
    are often admin-merged past the queue: a change under a package's
    directory otherwise lands without that package's deferred tests ever
    running, which is how #2109's `corpus_laws__test` expectations edit turned
    main red. The plan emits this list as `pr_tail_labels` and `buck2-tests`
    names it after its `-//:workspace_tail_tests` subtraction. Buck2 applies
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
    """The generated Buck2 dev-suite members and their test batches."""

    base = root if root is not None else REPO_ROOT
    inventory = json.loads((base / TARGET_INVENTORY).read_text(encoding="utf-8"))
    members = set(inventory["workspace_dev_test_targets"])
    batches = inventory["workspace_test_batches"]
    if not isinstance(batches, dict):
        raise ValueError("workspace_test_batches is not an object")
    return members, batches


def dependent_test_labels(
    query: set[str] | frozenset[str] | None, root: Path | None = None
) -> tuple[set[str], list[str], list[str]]:
    """Split a reverse-dependency query into (dev members, deferred, skipped).

    `query` is every label a Buck2 `rdeps` query returned for the touched
    packages, or None for a diff that can move any target. The pre-land gate
    runs what the diff can break without a service: the dev-suite members and
    the `dev-deferred` labels among them, which no other pre-land step runs.
    The rest of the inventory's executable tests among them are returned by
    name so the caller reports them: `manual` labels need a service or a
    Cargo recipe, and a `pr-deferred` suite is trunk work. Labels outside the
    inventory (feature variants, helper targets) are not tests of their own.
    """

    base = root if root is not None else REPO_ROOT
    inventory = json.loads((base / TARGET_INVENTORY).read_text(encoding="utf-8"))
    tests = {
        target["label"]
        for package in inventory["packages"]
        for target in package["targets"]
        if target.get("label") and target.get("kind") in EXECUTABLE_TEST_KINDS
    }
    selected = tests if query is None else tests & set(query)
    members = selected & set(inventory["workspace_dev_test_targets"])
    deferred = selected & set(
        _all_dev_deferred_labels(str(root) if root is not None else None)
    )
    return members, sorted(deferred), sorted(selected - members - deferred)


def package_build_labels(
    packages: set[str] | frozenset[str], root: Path | None = None
) -> dict[str, tuple[str, ...]]:
    """Exact generated build labels for the requested package directories."""

    base = root if root is not None else REPO_ROOT
    inventory = json.loads((base / TARGET_INVENTORY).read_text(encoding="utf-8"))
    wanted = {package.removeprefix("//") for package in packages}
    labels: dict[str, tuple[str, ...]] = {}
    for entry in inventory["packages"]:
        directory = PurePosixPath(entry["manifest"]).parent.as_posix()
        if directory not in wanted:
            continue
        exact = sorted(
            {
                target["build_label"]
                for target in entry["targets"]
                if isinstance(target.get("build_label"), str)
            }
        )
        labels[f"//{directory}"] = tuple(exact)
    return labels


def batch_labels(members: set[str], batches: dict[str, list[str]]) -> list[str]:
    labels = set(members)
    for batch, children in batches.items():
        # Partial reverse-dependency selections must not widen to other members.
        if set(children) <= members:
            labels.difference_update(children)
            labels.add(batch)
    return sorted(labels)


def affected_buck2_labels(
    scope: DevTestScope,
    members: set[str],
    tail: list[str],
    batches: Mapping[str, list[str]],
    package_builds: Mapping[str, tuple[str, ...]] | None = None,
    include_deferred: bool = True,
) -> tuple[list[str], list[str]]:
    """The (`buck2 test`, `buck2 build`) label lists one dev-test scope selects.

    This is the single affected-target selection: `scripts/dev-test.py` runs
    it locally and the pull-request leg of `buck2-tests` runs it in CI.
    `members` is the dev-inventory labels the caller selected (the touched
    packages', or a reverse-dependency query's); `tail` is the touched
    packages' `dev-deferred` labels (`pr_tail_labels`), plus the reverse
    dependencies' under dev-test's `--dependents` -- merge groups are the
    only events that run the tail job, so a pull request runs them itself, and
    the rule holds on a broad plan too: a package manifest widens the
    selection but is still a deferred test's input (#2109's corpus
    expectations file went red on main otherwise). A broad scope means the
    whole `//:dev_tests` suite; the facade seal rides a facade diff; a touched
    package no selected label covers gets its inventory build labels. The
    `//:schema_checks` build aggregate runs whenever either list is non-empty
    or the precise projection named a target or a file.
    `include_deferred` lets the local pre-land gate leave the tail to the
    hourly main run; CI keeps the default selection.
    """

    if not include_deferred:
        tail = []
    members = set(members) | set(tail)
    labels = ["//:dev_tests", *tail] if scope.broad else batch_labels(members, batches)
    if scope.facade:
        labels.append("//crates/lash:ui_fixtures")
        labels.append("//crates/lash:facade_completeness")
    uncovered = [
        package
        for package in scope.packages
        if not any(label.split(":")[0] == package for label in members)
    ]
    # Service-only and compile-only packages still need compilation proof,
    # including mixed diffs that also select tests in another package.
    if uncovered and not scope.broad:
        package_builds = package_builds or {}
        missing = sorted(package for package in uncovered if package not in package_builds)
        if missing:
            raise ValueError(f"target inventory has no build labels for: {', '.join(missing)}")
        builds = sorted(
            {label for package in uncovered for label in package_builds[package]}
        )
    else:
        builds = []
    if labels or builds or scope.targets or scope.files or scope.machinery:
        builds.append("//:schema_checks")
    return labels, builds


def pr_affected_targets(
    paths: list[str],
    root: Path | None = None,
    broad: bool = False,
    tail: list[str] | None = None,
) -> tuple[list[str], list[str]]:
    """The (`buck2 test`, `buck2 build`) label lists a pull request runs.

    The same selection `scripts/dev-test.py` computes for the diff: the
    package members of `dev_test_scope`, the touched packages' deferred
    labels, and the shared assembly in `affected_buck2_labels`. The script
    self-test inventory does not participate -- CI runs those proofs in
    `repo-gates`, not in the Buck2 partition. `broad` and `tail` let
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
    builds = package_build_labels(set(scope.packages), root)
    return affected_buck2_labels(scope, members, tail, batches, builds)


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
            f"//:dev_tests {tail} //crates/lash:ui_fixtures "
            "//crates/lash:facade_completeness"
        ),
        "pr_build_targets": "//:schema_checks",
    }
    outputs.update({family: "true" for family in FAMILIES})
    return outputs


def classify(
    changes: list[tuple[str, str]],
    event_name: str = "",
    workbench_dirs: frozenset[str] | None = None,
    store_dirs: frozenset[str] | None = None,
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
    # build input and turns on the Buck2 partition plus its path families.
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
        # The Buck2 labels the pull-request leg of `buck2-tests` runs: the
        # affected-target selection `scripts/dev-test.py` computes for the
        # same diff. Empty on a docs-only diff, which runs no Buck2 leg.
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
                return fail_open(f"affected Buck2 targets are underivable: {error}")
        outputs["pr_test_labels"] = " ".join(pr_tests)
        outputs["pr_build_targets"] = " ".join(pr_builds)
        return outputs
    # `rust` gates the Buck2 partition, which owns every agent-workbench unit
    # case, including browser projection with a pinned Node interpreter. The
    # breadth families stay off a workbench-only diff: the workbench is an
    # example host, not a store or a worker.
    breadth = bool(build) and not only_workbench
    try:
        pr_tests, pr_builds = pr_affected_targets(paths)
    except (OSError, ValueError, KeyError) as error:
        return fail_open(f"affected Buck2 targets are underivable: {error}")
    outputs["pr_test_labels"] = " ".join(pr_tests)
    outputs["pr_build_targets"] = " ".join(pr_builds)
    selected = {
        "rust": bool(build),
        "pr_pg_store": any(
            _is_pr_pg_store_path(path, classes[path]) for path in build
        ),
        "pr_host_restate": any(_is_pr_host_restate_path(path) for path in build),
        "functional_e2e": breadth,
        "workbench": workbench_hit,
        "regress": any(_is_regress_path(path) for path in build),
        "feature_lanes": any(
            _is_feature_gate_path(path, classes[path], lane_dirs) for path in build
        ),
        "schema": any(_is_schema_path(path) for path in build),
        "facade": any(_is_facade_path(path) for path in build),
        "tooling": any(_is_tooling_class(classes[path]) for path in build),
        # `stores` follows the Postgres store closure for merge groups and
        # dispatches. The narrower PR selectors are independent of that
        # closure.
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
    buck2_is_trusted: bool = True,
) -> list[str]:
    expected_jobs = UNGATED_JOBS | set(GATED_JOBS) | BUCK2_TEST_JOBS
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
    for selector in ("pr_pg_store", "pr_host_restate"):
        if plan_outputs.get(selector) not in {"true", "false"}:
            problems.append(
                f"plan output {selector} is {plan_outputs.get(selector)!r}, expected 'true' or 'false'"
            )
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

    for job in sorted(expected_jobs & set(needs)):
        result = needs[job].get("result")
        if job == "functional-e2e" and event_name in DEFERRED_EVENTS:
            wanted = (
                "success"
                if event_name == "pull_request"
                and plan_outputs.get("pr_host_restate") == "true"
                and buck2_is_trusted
                else "skipped"
            )
            if result != wanted:
                problems.append(f"{job} ended with {result!r} on a {event_name} event, expected {wanted}")
            continue
        if job in BUCK2_TEST_JOBS:
            rust_on = plan_outputs.get("rust") == "true"
            wanted = (
                "success" if buck2_is_trusted and rust_on
                and (job == BUCK2_TEST_JOB or event_name != "pull_request")
                else "skipped"
            )
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if buck2_is_trusted else 'untrusted'}"
                    f"{'' if rust_on else ' non-rust'} event,"
                    f" expected {wanted}"
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
            wanted = "success" if buck2_is_trusted and rust_on else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if buck2_is_trusted else 'untrusted'}"
                    f"{'' if rust_on else ' non-rust'} event,"
                    f" expected {wanted}"
                )
            continue
        if job == "check":
            # A trusted event seals the API inside `buck2-tests`; this Cargo
            # seal lane is the untrusted path, required when the facade moved.
            required = not buck2_is_trusted and plan_outputs.get("facade") == "true"
            wanted = "success" if required else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if buck2_is_trusted else 'untrusted'} event,"
                    f" expected {wanted}"
                )
            continue
        if job in POSTGRES_STORE_JOBS and event_name in DEFERRED_EVENTS:
            wanted = (
                "success"
                if (
                    event_name == "merge_group" and plan_outputs.get("stores") == "true"
                ) or (
                    event_name == "pull_request"
                    and plan_outputs.get("pr_pg_store") == "true"
                )
                else "skipped"
            )
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} on a {event_name} event, expected {wanted}"
                )
            continue
        if job == "workspace-tests":
            # On a trusted event the Buck2 partition owns every deterministic
            # Rust binary -- the agent-workbench Node-gated case included --
            # with a pinned Node interpreter. An untrusted event has no Buck2
            # partition and keeps the full Cargo workspace run.
            required = not buck2_is_trusted and plan_outputs.get("rust") == "true"
            wanted = "success" if required else "skipped"
            if result != wanted:
                problems.append(
                    f"{job} ended with {result!r} for a"
                    f" {'trusted' if buck2_is_trusted else 'untrusted'} event,"
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

    surface_parser = subparsers.add_parser(
        "versioned-surface",
        help="say whether a diff touches a registered versioned surface",
    )
    surface_parser.add_argument("--paths-file", type=Path, required=True)

    subparsers.add_parser("conclusion")
    args = parser.parse_args()

    if args.command == "versioned-surface":
        # Fail open: a diff or a marker that cannot be read runs the gate,
        # which then reports what is wrong with it.
        try:
            paths = [path for _, path in _read_nul_changes(args.paths_file)]
            selected = touches_versioned_surface(paths)
        except Exception as error:  # noqa: BLE001
            print(f"versioned-surface selection failed open: {error}", file=sys.stderr)
            selected = True
        _write_outputs({"versioned_surface": str(selected).lower()})
        return 0

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
    buck2_is_trusted = os.environ.get("BUCK2_TRUSTED")
    if buck2_is_trusted not in {"true", "false"}:
        print(
            f"Invalid BUCK2_TRUSTED: {buck2_is_trusted!r}, expected 'true' or 'false'",
            file=sys.stderr,
        )
        return 1
    problems = evaluate_conclusion(
        needs,
        os.environ.get("GITHUB_EVENT_NAME", ""),
        buck2_is_trusted == "true",
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
