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

from pathlib import Path
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]
WORKFLOW = ROOT / ".github" / "workflows" / "toolchain-canary.yml"


def document() -> dict[str, object]:
    return yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))


def triggers() -> dict[str, object]:
    # YAML 1.1 parses the bare `on:` key as boolean True.
    doc = document()
    return doc.get("on") or doc.get(True) or {}


class ToolchainCanaryTests(unittest.TestCase):
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
