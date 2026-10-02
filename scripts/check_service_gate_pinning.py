#!/usr/bin/env python3
"""Pin the ways a service-backed suite can silently stop running.

A suite that needs Postgres or S3 is worthless the moment it skips itself.
The mechanisms that keep it honest are one edit away from being lost, and
losing either one is invisible: the job still reports green, having compared
nothing.

Service-backed tests are ignored in runs without services. An invocation that
provisions a service must select them with `--run-ignored` for nextest or
`--include-ignored` / `--ignored` for libtest. Without that opt-in a selected
suite can report success without executing its laws. PostgreSQL fixtures require
their URL whenever a variant executes, so no separate environment flag is needed.

The check covers workflows, shell scripts and the justfile.

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

# The suites whose tests are `#[ignore]`d, and the flags that ask for them.
#
# Two ways a suite names itself. A separate integration target is selected by
# its test binary (`--test <binary>`); a suite that lives in a crate's own test
# module has no binary of its own and is selected by a test-name filter, which
# cargo and nextest both take as a bare positional argument. Both spellings are
# registered, because either one names a suite that runs zero tests and exits 0
# without the ignored opt-in.
#
# All matchers are deliberately token-exact rather than substring searches. A
# substring rule is one token away from being evaded in either direction:
# `--test=<binary>` names the same binary without ever containing
# "--test <binary>", and `--run-ignored default` contains "--run-ignored" while
# running exactly zero ignored tests. Either would have restored the defect this
# check exists to refuse, under a diff that reads like a no-op.
IGNORED_SUITE_BINARIES = ("cross_backend_store_differential",)

# nextest's `--run-ignored` takes a mode, and only these two run ignored tests;
# `default` runs none. libtest's `--include-ignored` takes no value.
NEXTEST_RUN_IGNORED = "--run-ignored"
NEXTEST_RUN_IGNORED_MODES = ("all", "ignored-only")
LIBTEST_INCLUDE_IGNORED = "--include-ignored"
# libtest's other opt-in: `--ignored` runs the ignored tests and nothing else,
# which is what a gate leg dedicated to one live suite asks for.
LIBTEST_IGNORED_ONLY = "--ignored"

# Files the ignored-suite rule sweeps beyond the workflows. The globs are recursive and cover
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


def names_ignored_suite(tokens: list[str]) -> str | None:
    """The ignored suite a tokenized command selects, if it selects one.

    A binary counts in both spellings, `--test <binary>` and `--test=<binary>`.
    """
    for index, token in enumerate(tokens):
        if token == "--test" and index + 1 < len(tokens):
            if tokens[index + 1] in IGNORED_SUITE_BINARIES:
                return tokens[index + 1]
            continue
        if token.startswith("--test=") and token.split("=", 1)[1] in IGNORED_SUITE_BINARIES:
            return token.split("=", 1)[1]
    return None


def requests_ignored_tests(tokens: list[str]) -> bool:
    """Whether a tokenized command actually asks for ignored tests to run.

    `--run-ignored` is only an opt-in at two of its three modes: `default`
    carries the flag and runs none of them, which is why the mode is checked
    rather than the flag's presence.
    """
    for index, token in enumerate(tokens):
        if token in (LIBTEST_INCLUDE_IGNORED, LIBTEST_IGNORED_ONLY):
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
    """The ignored-suite rule: naming the ignored suite obliges asking for ignored tests."""
    violations: list[Violation] = []
    modes = " or ".join(f"{NEXTEST_RUN_IGNORED} {mode}" for mode in NEXTEST_RUN_IGNORED_MODES)
    for location, text in fragments:
        for command in shell_commands(text):
            tokens = command.split()
            suite = names_ignored_suite(tokens)
            if suite is None:
                continue
            if requests_ignored_tests(tokens):
                continue
            violations.append(
                Violation(
                    path=str(path),
                    location=location,
                    detail=(
                        f"selects `{suite}` without {modes}, "
                        f"{LIBTEST_INCLUDE_IGNORED} or {LIBTEST_IGNORED_ONLY}; its tests are "
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
