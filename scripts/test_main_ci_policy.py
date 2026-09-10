#!/usr/bin/env python3
"""Executable contract tests for main CI supersession and release evidence."""

from __future__ import annotations

import ast
import json
import pathlib
import re
import subprocess
import sys
import tempfile
import textwrap
import unittest

import yaml


ROOT = pathlib.Path(__file__).resolve().parent.parent
CI_WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
EXPRESSION = re.compile(r"\$\{\{(.*?)\}\}", re.DOTALL)


class Context(dict):
    """Attribute-accessible GitHub expression context used by the fixtures."""

    def __getattr__(self, name: str) -> object:
        return self.get(name, "")


def context(value: object) -> object:
    if isinstance(value, dict):
        return Context({key: context(item) for key, item in value.items()})
    return value


def evaluate_expression(source: str, github: Context) -> object:
    """Evaluate the small expression grammar used by CI's concurrency policy."""

    translated = re.sub(
        r"\s+", " ", source.replace("&&", " and ").replace("||", " or ")
    ).strip()
    tree = ast.parse(translated, mode="eval")

    def evaluate(node: ast.AST) -> object:
        if isinstance(node, ast.Expression):
            return evaluate(node.body)
        if isinstance(node, ast.Constant) and isinstance(node.value, str):
            return node.value
        if isinstance(node, ast.Name) and node.id == "github":
            return github
        if isinstance(node, ast.Attribute) and not node.attr.startswith("_"):
            value = evaluate(node.value)
            if not isinstance(value, Context):
                raise ValueError(f"cannot read {node.attr!r} from {value!r}")
            return getattr(value, node.attr)
        if isinstance(node, ast.BoolOp) and isinstance(node.op, (ast.And, ast.Or)):
            result = evaluate(node.values[0])
            for value in node.values[1:]:
                if isinstance(node.op, ast.And):
                    if not result:
                        return result
                elif result:
                    return result
                result = evaluate(value)
            return result
        if isinstance(node, ast.Compare) and len(node.ops) == len(node.comparators) == 1:
            left = evaluate(node.left)
            right = evaluate(node.comparators[0])
            if isinstance(node.ops[0], ast.Eq):
                return left == right
            if isinstance(node.ops[0], ast.NotEq):
                return left != right
        if (
            isinstance(node, ast.Call)
            and isinstance(node.func, ast.Name)
            and node.func.id == "format"
            and not node.keywords
        ):
            values = [evaluate(argument) for argument in node.args]
            if not values or not isinstance(values[0], str):
                raise ValueError("format requires a string template")
            return values[0].format(*values[1:])
        raise ValueError(f"unsupported GitHub expression syntax: {ast.dump(node)}")

    return evaluate(tree)


def evaluate_template(template: str, github: Context) -> object:
    matches = tuple(EXPRESSION.finditer(template))
    if len(matches) == 1 and matches[0].span() == (0, len(template)):
        return evaluate_expression(matches[0].group(1), github)
    return EXPRESSION.sub(
        lambda match: str(evaluate_expression(match.group(1), github)), template
    )


def event_context(
    event_name: str,
    *,
    ref_name: str,
    head_ref: str = "",
    repository: str = "Ascending-AI/lash",
    pull_request_number: int = 0,
    pull_request_repo: str = "Ascending-AI/lash",
) -> Context:
    return context(
        {
            "workflow": "CI",
            "event_name": event_name,
            "ref_name": ref_name,
            "head_ref": head_ref,
            "repository": repository,
            "event": {
                "pull_request": {
                    "number": pull_request_number,
                    "head": {"repo": {"full_name": pull_request_repo}},
                }
            },
        }
    )


POLICY_CASES = (
    ("main push", event_context("push", ref_name="main"), "CI-trunk:push", True),
    (
        "current-main certification",
        event_context("workflow_dispatch", ref_name="main"),
        "CI-trunk:dispatch",
        False,
    ),
    (
        "older-main certification",
        event_context("workflow_dispatch", ref_name="release-certification/0123456789ab"),
        "CI-release-certification/0123456789ab",
        True,
    ),
    (
        "same-repository manual recovery",
        event_context("workflow_dispatch", ref_name="feature/a"),
        "CI-feature/a",
        True,
    ),
    (
        "same-repository pull request",
        event_context("pull_request", ref_name="123/merge", head_ref="feature/a"),
        "CI-feature/a",
        True,
    ),
    (
        "fork pull request",
        event_context(
            "pull_request",
            ref_name="123/merge",
            head_ref="main",
            pull_request_number=123,
            pull_request_repo="contributor/lash",
        ),
        "CI-fork-pr:123",
        True,
    ),
    (
        "merge queue",
        event_context(
            "merge_group", ref_name="gh-readonly-queue/main/pr-123-deadbeef"
        ),
        "CI-gh-readonly-queue/main/pr-123-deadbeef",
        False,
    ),
)


def policy_failures(group: str, cancel: str) -> list[str]:
    failures = []
    for name, github, expected_group, expected_cancel in POLICY_CASES:
        actual_group = evaluate_template(group, github)
        actual_cancel = evaluate_template(cancel, github)
        if actual_group != expected_group:
            failures.append(f"{name}: group {actual_group!r}, expected {expected_group!r}")
        if actual_cancel is not expected_cancel:
            failures.append(
                f"{name}: cancel-in-progress {actual_cancel!r}, expected {expected_cancel!r}"
            )
    return failures


def extract_release_gate(source: str) -> str:
    match = re.search(
        r'''python3 - "\$\{sha\}" "\$\{runs_file\}" <<'PY'\n'''
        r"(?P<body>.*?)^          PY$",
        source,
        re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise AssertionError("release workflow's exact-CI Python gate is missing")
    return textwrap.dedent(match.group("body"))


def run_release_gate(
    script: str, target: str, runs: list[dict[str, object]]
) -> subprocess.CompletedProcess[str]:
    with tempfile.TemporaryDirectory() as directory:
        fixture = pathlib.Path(directory) / "runs.json"
        fixture.write_text(json.dumps(runs), encoding="utf-8")
        return subprocess.run(
            [sys.executable, "-", target, str(fixture)],
            input=script,
            text=True,
            capture_output=True,
            check=False,
        )


def run(
    *,
    sha: str,
    event: str = "workflow_dispatch",
    status: str = "completed",
    conclusion: str | None = "success",
    database_id: int = 101,
) -> dict[str, object]:
    return {
        "headSha": sha,
        "event": event,
        "status": status,
        "conclusion": conclusion,
        "databaseId": database_id,
        "displayTitle": "CI fixture",
        "workflowName": "CI",
        "url": f"https://example.invalid/runs/{database_id}",
    }


def release_contract_failures(script: str) -> list[str]:
    target = "a" * 40
    other = "b" * 40
    cases = (
        ("exact successful dispatch", [run(sha=target)], True, ""),
        ("missing evidence", [], False, "no full-profile"),
        ("successful push only", [run(sha=target, event="push")], False, "no full-profile"),
        (
            "successful merge queue only",
            [run(sha=target, event="merge_group")],
            False,
            "no full-profile",
        ),
        ("wrong SHA", [run(sha=other)], False, "no full-profile"),
        ("cancelled", [run(sha=target, conclusion="cancelled")], False, "is cancelled"),
        ("failed", [run(sha=target, conclusion="failure")], False, "is failure"),
        (
            "still active",
            [run(sha=target, status="in_progress", conclusion=None)],
            False,
            "is in_progress",
        ),
        (
            "latest exact dispatch cancelled despite an older success",
            [
                run(sha=target, conclusion="cancelled", database_id=102),
                run(sha=target, database_id=101),
            ],
            False,
            "run 102",
        ),
    )
    failures = []
    for name, runs, accepted, diagnostic in cases:
        result = run_release_gate(script, target, runs)
        if (result.returncode == 0) is not accepted:
            failures.append(f"{name}: unexpected exit {result.returncode}: {result.stderr}")
        if diagnostic and diagnostic not in result.stderr:
            failures.append(f"{name}: missing diagnostic {diagnostic!r}: {result.stderr!r}")
    return failures


class MainCiPolicyTest(unittest.TestCase):
    @classmethod
    def setUpClass(cls) -> None:
        cls.ci_source = CI_WORKFLOW.read_text(encoding="utf-8")
        cls.ci = yaml.load(cls.ci_source, Loader=yaml.BaseLoader)
        cls.group = cls.ci["concurrency"]["group"]
        cls.cancel = cls.ci["concurrency"]["cancel-in-progress"]
        cls.release_gate = extract_release_gate(
            RELEASE_WORKFLOW.read_text(encoding="utf-8")
        )

    def test_ci_event_routes_and_concurrency_expressions_enforce_policy(self) -> None:
        triggers = self.ci["on"]
        self.assertEqual(["main"], triggers["push"]["branches"])
        self.assertIn("pull_request", triggers)
        self.assertIn("merge_group", triggers)
        self.assertIn("workflow_dispatch", triggers)
        self.assertEqual([], policy_failures(self.group, self.cancel))

    def test_concurrency_contract_rejects_red_mutations(self) -> None:
        mutants = {
            "old main runs do not cancel": self.cancel.replace(
                "github.event_name == 'push' || ", "", 1
            ),
            "certification collides with pushes": self.group.replace(
                "'trunk:dispatch'", "'trunk:push'", 1
            ),
            "merge queue entries cancel": self.cancel.replace(
                " }}", " || github.event_name == 'merge_group' }}", 1
            ),
        }
        for name, mutant in mutants.items():
            with self.subTest(name=name):
                group = mutant if "collides" in name else self.group
                cancel = mutant if "collides" not in name else self.cancel
                self.assertNotEqual([], policy_failures(group, cancel))

    def test_release_workflow_executes_exact_evidence_gate(self) -> None:
        self.assertEqual([], release_contract_failures(self.release_gate))

    def test_release_contract_rejects_red_mutations(self) -> None:
        mutants = (
            self.release_gate.replace(
                'run.get("event") == "workflow_dispatch"',
                'run.get("event") == "push"',
                1,
            ),
            self.release_gate.replace(
                'run.get("headSha") == sha', 'run.get("headSha") != sha', 1
            ),
            self.release_gate.replace(
                'run.get("conclusion") != "success"',
                'run.get("conclusion") == "success"',
                1,
            ),
        )
        for mutant in mutants:
            with self.subTest(mutant=mutant[:80]):
                self.assertNotEqual([], release_contract_failures(mutant))


if __name__ == "__main__":
    unittest.main()
