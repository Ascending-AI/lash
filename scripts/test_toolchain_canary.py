#!/usr/bin/env python3
"""Contract test: the toolchain canary (FIG-1684) stays a scheduled,
non-required drift signal.

Three properties carry the ticket's "done when": it runs on a weekly schedule,
it can never be a required check because it never triggers on pull_request or
merge_group, and a red run files a GitHub issue that names the offending lints
rather than leaving the signal on the Actions tab. A regression in any of the
three turns the canary into either noise or a silent dashboard.
"""

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "toolchain-canary.yml"
DRIFT_TITLE = "Toolchain canary: lint battery fails on latest stable"
PINNED_TITLE = "Toolchain canary: lint battery fails on pinned toolchain under Cargo"


def document() -> dict[str, object]:
    return yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))


def triggers() -> dict[str, object]:
    # YAML 1.1 parses the bare `on:` key as boolean True.
    doc = document()
    return doc.get("on") or doc.get(True) or {}


class ToolchainCanaryTests(unittest.TestCase):
    def report(
        self, condition: str, stable: str,
        existing: list[dict[str, object]] | None = None,
    ) -> list[list[str]]:
        """Execute the workflow shell and its jq filters without GitHub writes."""
        step = next(
            step for step in document()["jobs"]["canary"]["steps"]
            if step.get("if") == condition
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "rust-toolchain.toml").write_text('[toolchain]\nchannel = "1.98.1"\n')
            (root / "canary.log").write_text(
                "error: clippy::incompatible_msrv\n"
                "note: -D clippy::incompatible-msrv implied by -D warnings\n"
            )
            mock = root / "gh.py"
            mock.write_text(
                "import json, os, re, subprocess, sys\n"
                "with open(os.environ['GH_CALLS'], 'a') as log:\n"
                "    log.write(json.dumps(sys.argv[1:]) + '\\n')\n"
                "if sys.argv[1:3] == ['issue', 'list']:\n"
                "    args = sys.argv[1:]\n"
                "    issues = json.loads(os.environ['EXISTING_ISSUES'])\n"
                "    search = args[args.index('--search') + 1]\n"
                "    phrases = re.findall(r'\"([^\"]+)\"', search)\n"
                "    issues = [issue for issue in issues if all(\n"
                "        phrase.lower() in issue['title'].lower() for phrase in phrases)]\n"
                "    subprocess.run(['jq', '-r', args[args.index('--jq') + 1]],\n"
                "                   input=json.dumps(issues), text=True, check=True)\n"
            )
            calls = root / "calls.jsonl"
            result = subprocess.run(
                ["bash", "-c", """
                rustc() { printf 'rustc %s (test)\\n' "$STABLE"; }
                gh() { python3 "$GH_MOCK" "$@"; }
                """ + step["run"]],
                cwd=root, text=True, capture_output=True,
                env={**os.environ, "STABLE": stable, "GH_MOCK": str(mock),
                     "GH_CALLS": str(calls), "EXISTING_ISSUES": json.dumps(existing or []),
                     "GITHUB_SERVER_URL": "https://github.com",
                     "GITHUB_REPOSITORY": "Ascending-AI/lash", "GITHUB_RUN_ID": "123"},
            )
            self.assertEqual(1 if condition == "failure()" else 0, result.returncode,
                             result.stdout + result.stderr)
            return [json.loads(line) for line in calls.read_text().splitlines()]

    def test_pinned_failure_reports_cargo_lints_without_a_pin_bump(self) -> None:
        calls = self.report("failure()", "1.98.1")
        create = next(call for call in calls if call[:2] == ["issue", "create"])
        title = create[create.index("--title") + 1]
        body = create[create.index("--body") + 1]
        self.assertEqual(PINNED_TITLE, title)
        self.assertIn("under Cargo", body)
        self.assertNotIn("bumping the pin", body)
        self.assertNotIn("red on latest stable", body)
        self.assertEqual(["clippy::incompatible_msrv"],
                         [line for line in body.splitlines() if line.startswith("clippy::")])

    def test_drift_failure_names_latest_stable_and_the_pin_bump(self) -> None:
        calls = self.report("failure()", "1.99.0")
        create = next(call for call in calls if call[:2] == ["issue", "create"])
        self.assertEqual(DRIFT_TITLE,
                         create[create.index("--title") + 1])
        self.assertIn("bumping the pin", create[create.index("--body") + 1])

    def test_either_failure_title_reuses_an_open_issue(self) -> None:
        for title in (DRIFT_TITLE, PINNED_TITLE):
            with self.subTest(title=title):
                calls = self.report("failure()", "1.98.1", [
                    {"number": 9, "title": "Toolchain canary: lint battery fails elsewhere"},
                    {"number": 2387, "title": title},
                ])
                self.assertFalse(any(call[:2] == ["issue", "create"] for call in calls))
                self.assertTrue(any(call[:3] == ["issue", "comment", "2387"] for call in calls))
                edit = next(call for call in calls if call[:3] == ["issue", "edit", "2387"])
                self.assertEqual(PINNED_TITLE, edit[edit.index("--title") + 1])

    def test_recovery_closes_both_failure_titles(self) -> None:
        calls = self.report("success()", "1.98.1", [
            {"number": 2387, "title": DRIFT_TITLE},
            {"number": 2388, "title": PINNED_TITLE},
            {"number": 9, "title": "Toolchain canary: lint battery fails elsewhere"},
        ])
        self.assertEqual(["2387", "2388"],
                         [call[2] for call in calls if call[:2] == ["issue", "close"]])

    def test_runs_weekly_and_on_dispatch_only(self) -> None:
        on = triggers()
        self.assertIn("schedule", on)
        self.assertIn("workflow_dispatch", on)
        # Anything else would let the canary run where its failure could gate.
        self.assertEqual({"schedule", "workflow_dispatch"}, set(on))
        crons = [entry["cron"] for entry in on["schedule"]]
        self.assertTrue(
            all(len(cron.split()) == 5 for cron in crons),
            f"malformed cron entries: {crons}",
        )

    def test_lints_against_floating_stable_via_the_vendored_action(self) -> None:
        steps = [
            step
            for job in document()["jobs"].values()
            for step in job["steps"]
        ]
        toolchain_steps = [
            step
            for step in steps
            if str(step.get("uses", "")).startswith(
                "./.github/actions/rust-toolchain"
            )
        ]
        self.assertEqual(1, len(toolchain_steps))
        self.assertEqual("stable", toolchain_steps[0]["with"]["toolchain"])
        # The battery must fail on warnings: drift has to be loud.
        runs = "\n".join(str(step.get("run", "")) for step in steps)
        self.assertIn("cargo clippy", runs)
        self.assertIn("-D warnings", runs)

    def test_a_red_run_files_an_issue_and_green_closes_it(self) -> None:
        doc = document()
        self.assertEqual("write", doc["permissions"]["issues"])
        steps = [
            step
            for job in doc["jobs"].values()
            for step in job["steps"]
            if isinstance(step.get("run"), str)
        ]
        failure_steps = [
            step for step in steps if step.get("if") == "failure()"
        ]
        self.assertEqual(1, len(failure_steps))
        self.assertIn("gh issue create", failure_steps[0]["run"])
        # The issue has to name the lints, not just say "red".
        self.assertIn("clippy::", failure_steps[0]["run"])
        success_steps = [
            step for step in steps if step.get("if") == "success()"
        ]
        self.assertEqual(1, len(success_steps))
        self.assertIn("gh issue close", success_steps[0]["run"])


if __name__ == "__main__":
    unittest.main()
