#!/usr/bin/env python3
"""Derive main's Restate jobs from the suite registry and check their wiring."""

from __future__ import annotations

import argparse
import json
import os
import subprocess
import sys

from restate_suite import LEGS, ROOT, load_registry


def matrix() -> dict:
    return {"include": [
        {"suite": name, "leg": leg}
        for name in sorted(load_registry()) for leg in sorted(LEGS)
    ]}


def coverage_problems(rows: list[dict]) -> list[str]:
    expected = {(name, leg) for name in load_registry() for leg in LEGS}
    actual = [(row.get("suite"), row.get("leg")) for row in rows]
    problems = [f"unreached Restate leg: {name}/{leg}" for name, leg in sorted(expected - set(actual))]
    problems.extend(f"unknown Restate leg: {name}/{leg}" for name, leg in set(actual) - expected)
    if len(actual) != len(set(actual)):
        problems.append("duplicate Restate suite/leg jobs")
    return problems


def workflow_problems(workflow: dict) -> list[str]:
    jobs = workflow["jobs"]
    job = jobs.get("restate-suites", {})
    plan = jobs["plan"]
    problems = []
    if job.get("strategy", {}).get("matrix") != "${{ fromJSON(needs.plan.outputs.restate_matrix) }}":
        problems.append("Restate jobs do not consume the registry-derived matrix")
    if plan.get("outputs", {}).get("restate_matrix") != "${{ steps.restate-matrix.outputs.restate_matrix }}":
        problems.append("plan does not export the Restate matrix")
    producer = next((s for s in plan["steps"] if s.get("id") == "restate-matrix"), {})
    if producer.get("run") != 'python3 scripts/ci/restate_matrix.py matrix --github-output >> "${GITHUB_OUTPUT}"' or "if" in producer:
        problems.append("plan does not generate every registered Restate leg")
    if job.get("if") != "github.event_name == 'workflow_dispatch'":
        problems.append("Restate full-run jobs must run on dispatch and stay off the PR board")
    if job.get("needs") != "plan" or job.get("strategy", {}).get("fail-fast") is not False:
        problems.append("Restate jobs must depend on plan and run every matrix row after failures")
    parallel = job.get("strategy", {}).get("max-parallel", 0)
    if not 1 <= parallel <= 3:
        problems.append("Restate jobs must fit three service-backed slots")
    steps = job.get("steps", [])
    runner = next((s for s in steps if s.get("name") == "Run registered Restate leg"), {})
    if runner.get("run") != 'python3 scripts/ci/restate_matrix.py run "$SUITE" --leg "$LEG"':
        problems.append("Restate matrix rows do not reach the suite entrypoint")
    if runner.get("env") != {"SUITE": "${{ matrix.suite }}", "LEG": "${{ matrix.leg }}"}:
        problems.append("Restate runner does not receive both matrix coordinates")
    if "if" in runner or runner.get("continue-on-error") or job.get("continue-on-error"):
        problems.append("Restate legs must gate without conditional skips")
    if "restate-suites" not in jobs["ci-conclusion"]["needs"]:
        problems.append("CI conclusion does not require the Restate jobs")
    return problems + coverage_problems(matrix()["include"])


def run(name: str, leg: str) -> int:
    registry = load_registry()
    if name not in registry:
        raise SystemExit(f"unregistered Restate suite: {name}")
    env = {**os.environ, "LASH_RESTATE_SUITE_LEG": leg}
    driver = registry[name].get("ci_driver")
    if driver:
        command = ["bash", str(ROOT / driver)]
    else:
        # Give every suite the SQL backend. New PostgreSQL cases must execute
        # rather than silently return when their database is absent.
        command = ["bash", str(ROOT / "scripts/ci/with-service.sh"), "pg16", "--",
                   sys.executable, str(ROOT / "scripts/ci/restate_suite.py"),
                   "suite", name, "--leg", leg, "--keep-test-logs"]
    return subprocess.call(command, cwd=ROOT, env=env)


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    commands = parser.add_subparsers(dest="command", required=True)
    generate = commands.add_parser("matrix")
    generate.add_argument("--github-output", action="store_true")
    commands.add_parser("check")
    execute = commands.add_parser("run")
    execute.add_argument("name")
    execute.add_argument("--leg", choices=sorted(LEGS), required=True)
    args = parser.parse_args()
    if args.command == "matrix":
        print(("restate_matrix=" if args.github_output else "") + json.dumps(matrix(), separators=(",", ":")))
        return 0
    if args.command == "run":
        return run(args.name, args.leg)
    import yaml

    problems = workflow_problems(yaml.safe_load((ROOT / ".github/workflows/ci.yml").read_text()))
    for problem in problems:
        print(problem, file=sys.stderr)
    if not problems:
        print(f"Restate coverage: {len(matrix()['include'])} registered suite/leg jobs reached")
    return int(bool(problems))


if __name__ == "__main__":
    sys.exit(main())
