#!/usr/bin/env python3
"""Unit tests for ci_plan.py."""

from pathlib import Path
import unittest

import yaml

import ci_plan


CI_WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"
AGGREGATOR_ALLOWLIST = {"plan", "ci-conclusion", "linux-release-cache"}


def unregistered_ci_jobs(workflow_source: str) -> set[str]:
    workflow = yaml.safe_load(workflow_source)
    jobs = workflow["jobs"]
    aggregator_needs = jobs["ci-conclusion"]["needs"]
    return set(jobs) - set(aggregator_needs) - AGGREGATOR_ALLOWLIST


class ClassifyTests(unittest.TestCase):
    def test_docs_only_skips_every_expensive_family(self) -> None:
        plan = ci_plan.classify(
            [("M", "README.md"), ("A", "docs/runbooks/ci.md"), ("M", "runbooks/operator/README.md")]
        )
        self.assertEqual("true", plan["docs_only"])
        self.assertEqual({"false"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_docs_file_deletion_runs_every_expensive_family(self) -> None:
        plan = ci_plan.classify([("D", "docs/adr/0008-confidence-gate.md")])
        self.assertEqual("false", plan["docs_only"])
        self.assertEqual("docs deletion", plan["reason"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_docs_addition_and_modification_preserve_docs_only_skip(self) -> None:
        plan = ci_plan.classify([("A", "docs/new.md"), ("M", "CONTEXT.md")])
        self.assertEqual("true", plan["docs_only"])
        self.assertEqual("docs-only diff", plan["reason"])
        self.assertEqual({"false"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_docs_markdown_file_stays_docs_only(self) -> None:
        plan = ci_plan.classify([("M", "docs/adr/0079-x.md")])
        self.assertEqual("true", plan["docs_only"])
        self.assertEqual({"false"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_api_surface_snapshot_runs_every_expensive_family(self) -> None:
        plan = ci_plan.classify([("M", "docs/api-surface.snapshot")])
        self.assertEqual("false", plan["docs_only"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_api_example_coverage_fixture_runs_every_expensive_family(self) -> None:
        plan = ci_plan.classify([("M", "docs/api-example-coverage.toml")])
        self.assertEqual("false", plan["docs_only"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_extensionless_docs_path_runs_every_expensive_family(self) -> None:
        plan = ci_plan.classify([("M", "docs/CNAME")])
        self.assertEqual("false", plan["docs_only"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_docs_deletion_mixed_with_docs_modification_runs_everything(self) -> None:
        plan = ci_plan.classify([("D", "docs/old.md"), ("M", "README.md")])
        self.assertEqual("false", plan["docs_only"])
        self.assertEqual("docs deletion", plan["reason"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_unknown_status_fails_open(self) -> None:
        plan = ci_plan.classify([("X", "docs/unknown.md")])
        self.assertEqual("true", plan["fail_open"])
        self.assertIn("unknown change statuses", plan["reason"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_rust_change_runs_every_expensive_family(self) -> None:
        plan = ci_plan.classify([("M", "crates/lash-core/src/lib.rs")])
        self.assertEqual("true", plan["rust_code"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_readme_prefixed_rust_file_runs_every_expensive_family(self) -> None:
        plan = ci_plan.classify([("A", "crates/x/src/readme_gen.rs")])
        self.assertEqual("false", plan["docs_only"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_each_global_invalidator_runs_everything(self) -> None:
        paths = [
            "Cargo.lock",
            "crates/lash-core/Cargo.toml",
            "rust-toolchain.toml",
            ".cargo/config.toml",
            ".config/nextest.toml",
            ".github/workflows/ci.yml",
            "scripts/lint_docs.py",
            "justfile",
            "deny.toml",
        ]
        for path in paths:
            with self.subTest(path=path):
                plan = ci_plan.classify([("M", path)])
                self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_unknown_path_fails_open(self) -> None:
        plan = ci_plan.classify([("M", "mystery.data")])
        self.assertEqual("true", plan["fail_open"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})


def successful_needs() -> dict[str, dict[str, object]]:
    plan_outputs = {family: "true" for family in ci_plan.FAMILIES}
    plan_outputs.update({"docs_only": "false", "fail_open": "false"})
    needs = {job: {"result": "success", "outputs": {}} for job in ci_plan.UNGATED_JOBS | set(ci_plan.GATED_JOBS)}
    # Full-profile jobs are skipped on every event but workflow_dispatch,
    # which is what these event-less fixtures exercise.
    for job in ci_plan.FULL_PROFILE_JOBS:
        needs[job] = {"result": "skipped", "outputs": {}}
    needs["plan"]["outputs"] = plan_outputs
    return needs


class ConclusionTests(unittest.TestCase):
    def test_all_success_succeeds(self) -> None:
        self.assertEqual([], ci_plan.evaluate_conclusion(successful_needs()))

    def test_failure_and_cancellation_fail(self) -> None:
        for result in ("failure", "cancelled"):
            with self.subTest(result=result):
                needs = successful_needs()
                needs["test-shard"]["result"] = result
                self.assertTrue(ci_plan.evaluate_conclusion(needs))

    def test_planned_skip_succeeds(self) -> None:
        needs = successful_needs()
        needs["plan"]["outputs"].update({"docs_only": "true", **{family: "false" for family in ci_plan.FAMILIES}})
        for job in ci_plan.GATED_JOBS:
            needs[job]["result"] = "skipped"
        self.assertEqual([], ci_plan.evaluate_conclusion(needs))

    def test_wrongly_skipped_job_fails(self) -> None:
        needs = successful_needs()
        needs["test-shard"]["result"] = "skipped"
        problems = ci_plan.evaluate_conclusion(needs)
        self.assertTrue(any("required it to run" in problem for problem in problems))

    def test_inconsistent_classifier_output_fails(self) -> None:
        needs = successful_needs()
        needs["plan"]["outputs"].update({"docs_only": "false", "fail_open": "false", "rust": "false"})
        needs["test-shard"]["result"] = "skipped"
        problems = ci_plan.evaluate_conclusion(needs)
        self.assertTrue(any("wrongly skipped" in problem for problem in problems))

    def test_skipped_ungated_job_fails(self) -> None:
        needs = successful_needs()
        needs["facade-only-examples"]["result"] = "skipped"
        self.assertTrue(ci_plan.evaluate_conclusion(needs))

    def test_missing_needed_job_fails(self) -> None:
        needs = successful_needs()
        del needs["facade-only-examples"]
        self.assertTrue(ci_plan.evaluate_conclusion(needs))


class WorkflowRegistrationTests(unittest.TestCase):
    def test_every_ci_job_is_registered_or_allowlisted(self) -> None:
        self.assertEqual(set(), unregistered_ci_jobs(CI_WORKFLOW.read_text(encoding="utf-8")))

    def test_rogue_job_is_caught(self) -> None:
        workflow_copy = CI_WORKFLOW.read_text(encoding="utf-8").rstrip()
        workflow_copy += "\n\n  rogue-job:\n    runs-on: ubuntu-latest\n"
        self.assertEqual({"rogue-job"}, unregistered_ci_jobs(workflow_copy))


if __name__ == "__main__":
    unittest.main()
