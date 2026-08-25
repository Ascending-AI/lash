#!/usr/bin/env python3
"""Unit tests for ci_plan.py."""

import unittest

import ci_plan


class ClassifyTests(unittest.TestCase):
    def test_docs_only_skips_every_expensive_family(self) -> None:
        plan = ci_plan.classify(["README.md", "docs/runbooks/ci.md", "runbooks/operator/README.md"])
        self.assertEqual("true", plan["docs_only"])
        self.assertEqual({"false"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_rust_change_runs_every_expensive_family(self) -> None:
        plan = ci_plan.classify(["crates/lash-core/src/lib.rs"])
        self.assertEqual("true", plan["rust_code"])
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
                plan = ci_plan.classify([path])
                self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_unknown_path_fails_open(self) -> None:
        plan = ci_plan.classify(["mystery.data"])
        self.assertEqual("true", plan["fail_open"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})


def successful_needs() -> dict[str, dict[str, object]]:
    plan_outputs = {family: "true" for family in ci_plan.FAMILIES}
    plan_outputs.update({"docs_only": "false", "fail_open": "false"})
    needs = {job: {"result": "success", "outputs": {}} for job in ci_plan.UNGATED_JOBS | set(ci_plan.GATED_JOBS)}
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
        needs["lint"]["result"] = "skipped"
        self.assertTrue(ci_plan.evaluate_conclusion(needs))

    def test_missing_needed_job_fails(self) -> None:
        needs = successful_needs()
        del needs["functional-e2e"]
        self.assertTrue(ci_plan.evaluate_conclusion(needs))


if __name__ == "__main__":
    unittest.main()
