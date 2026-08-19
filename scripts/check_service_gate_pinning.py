#!/usr/bin/env python3
"""Pin the two ways a service-backed suite can silently stop running.

A suite that needs Postgres or MinIO is worthless the moment it skips itself.
Both mechanisms that keep it honest are one edit away from being lost, and
losing either one is invisible: the job still reports green, having compared
nothing.

Rule 1 -- the require flag. A workflow scope that hands a suite
``LASH_POSTGRES_DATABASE_URL`` (or ``LASH_MINIO_ENDPOINT``) must also set the
matching ``LASH_REQUIRE_*`` flag to ``"1"``, in that scope or an enclosing one.
The flag is what turns "the service is missing" from a skip into a failure, so
a job that provisions a service and forgets the flag skips green whenever the
service fails to start -- which is exactly what the release and perf legs did
before FIG-1217.

Rule 2 -- the ignored-suite opt-in. The cross-backend differential's tests are
``#[ignore]``d so a bare local run reports them as skipped instead of passing
without comparing anything. That makes every invocation that *does* provision a
service responsible for asking for them by name: ``--run-ignored`` for nextest,
``--include-ignored`` for a libtest harness. An invocation that names the test
binary without either flag runs zero of its tests and still exits 0.

Scope, stated rather than implied. Rule 1 covers GitHub workflow ``env``
mappings only. Shell scripts also export these variables, but they do so around
commands that legitimately need the URL without the test-harness flag -- the
``run-postgres`` CLI takes a database and is not a suite that can skip -- and a
rule with exemptions is a rule the next author edits around. Workflows are
declarative and exception-free, and "removing it from CI" is the regression the
ticket names. Rule 2 has no such ambiguity and covers workflows, shell scripts,
and the justfile alike.

Only the standard library plus PyYAML is used, matching the sibling checks.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import re
import sys
from typing import Iterable

import yaml


ROOT = Path(__file__).resolve().parents[1]

# The env pairs a workflow must keep together: providing the first without the
# second is what lets a suite skip itself green.
REQUIRE_FLAG_PAIRS = {
    "LASH_POSTGRES_DATABASE_URL": "LASH_REQUIRE_POSTGRES",
    "LASH_MINIO_ENDPOINT": "LASH_REQUIRE_MINIO",
}

# The test binary whose tests are `#[ignore]`d, and the flags that ask for them.
#
# Both matchers are deliberately token-exact rather than substring searches. A
# substring rule is one token away from being evaded in either direction:
# `--test=<binary>` names the same binary without ever containing
# "--test <binary>", and `--run-ignored default` contains "--run-ignored" while
# running exactly zero ignored tests. Either would have restored the defect this
# check exists to refuse, under a diff that reads like a no-op.
IGNORED_SUITE_BINARY = "cross_backend_store_differential"

# nextest's `--run-ignored` takes a mode, and only these two run ignored tests;
# `default` runs none. libtest's `--include-ignored` takes no value.
NEXTEST_RUN_IGNORED = "--run-ignored"
NEXTEST_RUN_IGNORED_MODES = ("all", "ignored-only")
LIBTEST_INCLUDE_IGNORED = "--include-ignored"

# Files rule 2 sweeps beyond the workflows. The globs are recursive and cover
# both YAML spellings so a script or workflow filed one directory deeper does
# not quietly leave the sweep.
SHELL_GLOBS = ("scripts/**/*.sh",)
EXTRA_FILES = ("justfile",)


@dataclass(frozen=True)
class Violation:
    path: str
    location: str
    detail: str


def workflow_paths(root: Path) -> tuple[Path, ...]:
    directory = root / ".github" / "workflows"
    return tuple(
        sorted(
            path
            for pattern in ("*.yml", "*.yaml")
            for path in directory.glob(pattern)
            if path.is_file()
        )
    )


def env_mapping(node: object) -> dict[str, str]:
    """The literal string env pairs of a workflow node, if it declares any."""
    if not isinstance(node, dict):
        return {}
    raw = node.get("env")
    if not isinstance(raw, dict):
        return {}
    return {
        str(key): str(value)
        for key, value in raw.items()
        if isinstance(key, str) and not isinstance(value, (dict, list))
    }


def check_require_flags(path: Path, document: object) -> list[Violation]:
    """Rule 1: a provisioned service env must carry its require flag."""
    violations: list[Violation] = []
    if not isinstance(document, dict):
        return violations
    workflow_env = env_mapping(document)
    jobs = document.get("jobs")
    if not isinstance(jobs, dict):
        jobs = {}

    def inspect(scope: dict[str, str], location: str) -> None:
        for url_key, flag_key in REQUIRE_FLAG_PAIRS.items():
            if url_key not in scope:
                continue
            if scope.get(flag_key) != "1":
                found = scope.get(flag_key)
                seen = "unset" if found is None else repr(found)
                violations.append(
                    Violation(
                        path=str(path),
                        location=location,
                        detail=(
                            f"sets {url_key} but {flag_key} is {seen}; a service "
                            f'that fails to start would skip this leg green. Set '
                            f'{flag_key}: "1" in this scope or an enclosing one.'
                        ),
                    )
                )

    inspect(workflow_env, "workflow env")
    for job_name, job in jobs.items():
        if not isinstance(job, dict):
            continue
        job_scope = {**workflow_env, **env_mapping(job)}
        inspect(job_scope, f"job `{job_name}` env")
        steps = job.get("steps")
        if not isinstance(steps, list):
            continue
        for index, step in enumerate(steps, start=1):
            if not isinstance(step, dict):
                continue
            step_env = env_mapping(step)
            if not step_env:
                continue
            label = step.get("name") if isinstance(step.get("name"), str) else None
            where = f"job `{job_name}` step {index}"
            if label:
                where += f" (`{label}`)"
            inspect({**job_scope, **step_env}, f"{where} env")
    return violations


def shell_commands(text: str) -> tuple[str, ...]:
    """Split a shell fragment into commands, joining line continuations first.

    A backslash continuation is what a multi-line `cargo` invocation uses, so
    the flags on its last line belong to the same command as the `--test`
    argument on an earlier one. Joining first is what makes the rule exact
    rather than a per-file occurrence count that any unrelated flag could
    satisfy.
    """
    joined = re.sub(r"\\\n[ \t]*", " ", text)
    parts = re.split(r"[\n;]|&&|\|\|", joined)
    return tuple(part.strip() for part in parts if part.strip())


def names_ignored_suite(tokens: list[str]) -> bool:
    """Whether a tokenized command selects the ignored suite's test binary.

    Both spellings count: `--test <binary>` and `--test=<binary>`.
    """
    for index, token in enumerate(tokens):
        if token == "--test" and index + 1 < len(tokens):
            if tokens[index + 1] == IGNORED_SUITE_BINARY:
                return True
        elif token == f"--test={IGNORED_SUITE_BINARY}":
            return True
    return False


def requests_ignored_tests(tokens: list[str]) -> bool:
    """Whether a tokenized command actually asks for ignored tests to run.

    `--run-ignored` is only an opt-in at two of its three modes: `default`
    carries the flag and runs none of them, which is why the mode is checked
    rather than the flag's presence.
    """
    for index, token in enumerate(tokens):
        if token == LIBTEST_INCLUDE_IGNORED:
            return True
        if token == NEXTEST_RUN_IGNORED and index + 1 < len(tokens):
            if tokens[index + 1] in NEXTEST_RUN_IGNORED_MODES:
                return True
        elif token.startswith(f"{NEXTEST_RUN_IGNORED}="):
            if token.split("=", 1)[1] in NEXTEST_RUN_IGNORED_MODES:
                return True
    return False


def check_ignored_suite_commands(
    path: Path, fragments: Iterable[tuple[str, str]]
) -> list[Violation]:
    """Rule 2: naming the ignored suite obliges asking for ignored tests."""
    violations: list[Violation] = []
    modes = " or ".join(f"{NEXTEST_RUN_IGNORED} {mode}" for mode in NEXTEST_RUN_IGNORED_MODES)
    for location, text in fragments:
        for command in shell_commands(text):
            tokens = command.split()
            if not names_ignored_suite(tokens):
                continue
            if requests_ignored_tests(tokens):
                continue
            violations.append(
                Violation(
                    path=str(path),
                    location=location,
                    detail=(
                        f"runs the cross-backend differential without {modes} "
                        f"or {LIBTEST_INCLUDE_IGNORED}; its tests are "
                        "`#[ignore]`d, so this invocation runs none of them and "
                        "still exits 0."
                    ),
                )
            )
    return violations


def workflow_run_fragments(document: object) -> tuple[tuple[str, str], ...]:
    """Every `run:` script in a workflow, with a human location for each."""
    fragments: list[tuple[str, str]] = []
    if not isinstance(document, dict):
        return ()
    jobs = document.get("jobs")
    if not isinstance(jobs, dict):
        return ()
    for job_name, job in jobs.items():
        if not isinstance(job, dict):
            continue
        steps = job.get("steps")
        if not isinstance(steps, list):
            continue
        for index, step in enumerate(steps, start=1):
            if not isinstance(step, dict):
                continue
            run = step.get("run")
            if not isinstance(run, str):
                continue
            label = step.get("name") if isinstance(step.get("name"), str) else None
            where = f"job `{job_name}` step {index}"
            if label:
                where += f" (`{label}`)"
            fragments.append((f"{where} run", run))
    return tuple(fragments)


def check_repository(root: Path) -> list[Violation]:
    violations: list[Violation] = []
    for path in workflow_paths(root):
        try:
            document = yaml.safe_load(path.read_text(encoding="utf-8"))
        except yaml.YAMLError as error:
            violations.append(
                Violation(str(path), "document", f"cannot parse workflow: {error}")
            )
            continue
        violations.extend(check_require_flags(path, document))
        violations.extend(
            check_ignored_suite_commands(path, workflow_run_fragments(document))
        )

    shell_paths: list[Path] = []
    for pattern in SHELL_GLOBS:
        shell_paths.extend(sorted(root.glob(pattern)))
    for name in EXTRA_FILES:
        candidate = root / name
        if candidate.is_file():
            shell_paths.append(candidate)
    for path in shell_paths:
        text = path.read_text(encoding="utf-8")
        violations.extend(check_ignored_suite_commands(path, (("file", text),)))
    return violations


def parse_args(argv: list[str]) -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--repo", type=Path, default=ROOT, help=argparse.SUPPRESS)
    return parser.parse_args(argv)


def main(argv: list[str] | None = None) -> int:
    args = parse_args(argv if argv is not None else sys.argv[1:])
    violations = check_repository(args.repo)
    if violations:
        print("service-gate pinning check failed:", file=sys.stderr)
        for violation in violations:
            print(
                f"- {violation.path}: {violation.location}: {violation.detail}",
                file=sys.stderr,
            )
        return 1
    print("service-gate pinning check passed")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
