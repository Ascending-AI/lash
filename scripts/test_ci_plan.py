#!/usr/bin/env python3
"""Unit tests for ci_plan.py."""

from pathlib import Path
import unittest

import yaml

import ci_plan


CI_WORKFLOW = Path(__file__).resolve().parents[1] / ".github/workflows/ci.yml"
AGGREGATOR_ALLOWLIST = {"plan", "ci-conclusion"}


def unregistered_ci_jobs(workflow_source: str) -> set[str]:
    workflow = yaml.safe_load(workflow_source)
    jobs = workflow["jobs"]
    aggregator_needs = jobs["ci-conclusion"]["needs"]
    return set(jobs) - set(aggregator_needs) - AGGREGATOR_ALLOWLIST


class ConfidenceConclusionTests(unittest.TestCase):
    def needs(self, event="workflow_dispatch", selector="full"):
        return {job: {"result": "success" if event in {"schedule", "workflow_dispatch"}
                      and ((selector != "full") if policy == "selector" else selector == "full")
                      else "skipped"} for job, policy in ci_plan.CONFIDENCE_JOB_POLICY.items()}

    def test_confidence_job_policy_matches_workflow_and_producer_edges(self):
        jobs = yaml.safe_load(CI_WORKFLOW.with_name("confidence.yml").read_text())["jobs"]
        self.assertEqual(set(jobs) - {"confidence-conclusion"}, set(ci_plan.CONFIDENCE_JOB_POLICY))
        self.assertEqual(set(ci_plan.CONFIDENCE_JOB_POLICY), set(jobs["confidence-conclusion"]["needs"]))
        self.assertEqual("full-producer", ci_plan.CONFIDENCE_JOB_POLICY["confidence-build"])
        for job, policy in ci_plan.CONFIDENCE_JOB_POLICY.items():
            if policy == "full-consumer":
                self.assertEqual("confidence-build", jobs[job]["needs"])

    def test_confidence_event_and_selector_policy(self):
        for event in ("schedule", "workflow_dispatch", "push", "merge_group"):
            for selector in (("full", "full+area:sim", "fast") if event != "schedule" else ("full",)):
                with self.subTest(event=event, selector=selector):
                    self.assertEqual([], ci_plan.evaluate_confidence_conclusion(self.needs(event, selector), event, selector))

    def test_every_confidence_stage_fails_closed(self):
        for job in ci_plan.CONFIDENCE_JOB_POLICY:
            selector = "fast" if job == "confidence" else "full"
            for result in ("failure", "cancelled", "skipped", None, "missing"):
                with self.subTest(job=job, result=result):
                    needs = self.needs(selector=selector)
                    if result == "missing":
                        del needs[job]
                    else:
                        needs[job]["result"] = result
                    self.assertTrue(any(job in p for p in ci_plan.evaluate_confidence_conclusion(needs, "workflow_dispatch", selector)))

    def test_confidence_rejects_unknown_jobs_policies_and_events(self):
        from unittest.mock import patch
        needs = self.needs()
        needs["unmapped"] = {"result": "success"}
        self.assertTrue(ci_plan.evaluate_confidence_conclusion(needs, "workflow_dispatch", "full"))
        with patch.dict(ci_plan.CONFIDENCE_JOB_POLICY, {"confidence-build": "unknown"}):
            self.assertTrue(ci_plan.evaluate_confidence_conclusion(self.needs(), "workflow_dispatch", "full"))
        self.assertTrue(ci_plan.evaluate_confidence_conclusion(self.needs(), "unknown", "full"))
        self.assertTrue(ci_plan.evaluate_confidence_conclusion(self.needs(), "schedule", "fast"))
        self.assertTrue(ci_plan.evaluate_confidence_conclusion(self.needs(), "workflow_dispatch", ""))


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
            "scripts/gate_scope.py",
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
    needs["plan"]["outputs"] = plan_outputs
    return needs


def apply_event_deferrals(needs: dict, event: str) -> dict:
    """Give every event-deferred job the result `event` expects of it.

    Trunk-only jobs skip on both deferred events; the queue-required compile
    lanes skip on pull requests only and must succeed in the queue.
    """

    for job in ci_plan.TRUNK_ONLY_JOBS:
        needs[job]["result"] = "skipped" if event in ci_plan.DEFERRED_EVENTS else "success"
    for job in ci_plan.QUEUE_REQUIRED_COMPILE_JOBS:
        needs[job]["result"] = "skipped" if event == "pull_request" else "success"
    return needs


class ConclusionTests(unittest.TestCase):
    def test_hygiene_jobs_are_required_for_every_event_and_docs_changes(self) -> None:
        workflow = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]
        for job in ("diff-hygiene", "secret-scan"):
            self.assertIn(job, ci_plan.UNGATED_JOBS)
            self.assertIn(job, workflow["ci-conclusion"]["needs"])
            self.assertNotIn("if", workflow[job])
            for event in ("pull_request", "merge_group", "push", "workflow_dispatch"):
                for result in ("failure", "cancelled", "skipped"):
                    with self.subTest(job=job, event=event, result=result):
                        needs = successful_needs()
                        needs["plan"]["outputs"].update({"docs_only": "true", **{f: "false" for f in ci_plan.FAMILIES}})
                        for gated in ci_plan.GATED_JOBS:
                            needs[gated]["result"] = "skipped"
                        needs[job]["result"] = result
                        problems = ci_plan.evaluate_conclusion(needs, event_name=event)
                        self.assertTrue(any(job in problem for problem in problems))

    def test_all_success_succeeds(self) -> None:
        self.assertEqual([], ci_plan.evaluate_conclusion(successful_needs()))

    def test_failure_and_cancellation_fail(self) -> None:
        for result in ("failure", "cancelled"):
            with self.subTest(result=result):
                needs = successful_needs()
                needs["workspace-tests"]["result"] = result
                self.assertTrue(ci_plan.evaluate_conclusion(needs))

    def test_planned_skip_succeeds(self) -> None:
        needs = successful_needs()
        needs["plan"]["outputs"].update({"docs_only": "true", **{family: "false" for family in ci_plan.FAMILIES}})
        for job in ci_plan.GATED_JOBS:
            needs[job]["result"] = "skipped"
        self.assertEqual([], ci_plan.evaluate_conclusion(needs))

    def test_wrongly_skipped_job_fails(self) -> None:
        needs = successful_needs()
        needs["workspace-tests"]["result"] = "skipped"
        problems = ci_plan.evaluate_conclusion(needs)
        self.assertTrue(any("required it to run" in problem for problem in problems))

    def test_inconsistent_classifier_output_fails(self) -> None:
        needs = successful_needs()
        needs["plan"]["outputs"].update({"docs_only": "false", "fail_open": "false", "rust": "false"})
        needs["workspace-tests"]["result"] = "skipped"
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


class ProducerConclusionTests(unittest.TestCase):
    def event_needs(self, event, enabled=True):
        needs = successful_needs()
        apply_event_deferrals(needs, event)
        if not enabled:
            for job in ci_plan.WORKERS_E2E_JOBS:
                needs[job]["result"] = "skipped"
        self.assertEqual([], self.evaluate(needs, event, enabled))
        return needs

    def evaluate(self, needs, event, enabled=True):
        return ci_plan.evaluate_conclusion(needs, event, "refs/heads/main", enabled)

    def assert_producer_rejected(self, event, result):
        for producer in ("worker-artifacts",):
            with self.subTest(producer=producer):
                needs = self.event_needs(event)
                needs[producer]["result"] = result
                self.assertTrue(any(producer in p for p in self.evaluate(needs, event)))

    def test_push_main_producer_failure_rejected(self):
        self.assert_producer_rejected("push", "failure")

    def test_push_main_producer_cancelled_rejected(self):
        self.assert_producer_rejected("push", "cancelled")

    def test_push_main_producer_skipped_rejected(self):
        self.assert_producer_rejected("push", "skipped")

    def test_dispatch_producer_failure_rejected(self):
        self.assert_producer_rejected("workflow_dispatch", "failure")

    def test_dispatch_producer_cancelled_rejected(self):
        self.assert_producer_rejected("workflow_dispatch", "cancelled")

    def test_dispatch_producer_skipped_rejected(self):
        self.assert_producer_rejected("workflow_dispatch", "skipped")

    def test_labeled_pr_producer_failure_rejected(self):
        self.assert_producer_rejected("pull_request", "failure")

    def test_labeled_pr_producer_cancelled_rejected(self):
        self.assert_producer_rejected("pull_request", "cancelled")

    def test_labeled_pr_producer_skipped_rejected(self):
        self.assert_producer_rejected("pull_request", "skipped")

    def test_skipped_consumer_cascade_rejected(self):
        for event in ("push", "workflow_dispatch", "pull_request"):
            for consumer in ("workspace-tests", "restate-postgres-workers", "restate-postgres-workers-summary"):
                with self.subTest(event=event, consumer=consumer):
                    needs = self.event_needs(event)
                    needs[consumer]["result"] = "skipped"
                    self.assertTrue(any(consumer in p for p in self.evaluate(needs, event)))

    def test_process_operations_consumer_failure_cancelled_and_skipped_rejected(self):
        for event in ("push", "workflow_dispatch"):
            for result in ("failure", "cancelled", "skipped"):
                with self.subTest(event=event, result=result):
                    needs = self.event_needs(event)
                    needs["functional-e2e-process-operations"]["result"] = result
                    self.assertTrue(any("functional-e2e-process-operations" in p
                                        for p in self.evaluate(needs, event)))

    def test_worker_producer_failure_cascading_to_process_operations_rejected(self):
        for event in ("push", "workflow_dispatch"):
            needs = self.event_needs(event)
            needs["worker-artifacts"]["result"] = "failure"
            needs["functional-e2e-process-operations"]["result"] = "skipped"
            problems = self.evaluate(needs, event)
            for job in ("worker-artifacts", "functional-e2e-process-operations"):
                self.assertTrue(any(job in p for p in problems))

    def test_unlabeled_pr_worker_producer_skipped_accepted(self):
        needs = self.event_needs("pull_request", False)
        self.assertEqual("skipped", needs["worker-artifacts"]["result"])
        self.assertEqual([], self.evaluate(needs, "pull_request", False))

    def test_merge_group_worker_producer_and_segments_skipped_accepted(self):
        needs = self.event_needs("merge_group", False)
        for job in ("worker-artifacts", "restate-postgres-workers", "restate-postgres-workers-summary"):
            self.assertEqual("skipped", needs[job]["result"])
        self.assertEqual([], self.evaluate(needs, "merge_group", False))


POSTGRES_TEST_STEPS = {
    "Test PostgreSQL catalog compatibility",
    "Test Postgres store (conformance and attempt atomicity)",
    "Test runtime pool-wait binding",
    "Test runtime Postgres agent scenarios",
    "Test cross-backend store differential",
}


def selected_postgres_test_steps(event: str, role: str) -> set[str]:
    """Evaluate the small fixed condition vocabulary used by the PG matrix."""

    pr_class = event in {"pull_request", "merge_group"}
    selectors = {
        "matrix.role == 'compatibility'": role == "compatibility",
        "matrix.role == 'primary'": role == "primary",
        (
            "matrix.role == 'primary' && github.event_name != 'pull_request'"
            " && github.event_name != 'merge_group'"
        ): role == "primary" and not pr_class,
    }
    job = yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))["jobs"][
        "postgres-store"
    ]
    selected = set()
    for step in job["steps"]:
        if step.get("name") not in POSTGRES_TEST_STEPS:
            continue
        condition = step.get("if")
        if condition not in selectors:
            raise AssertionError(f"unmodelled PostgreSQL test condition: {condition!r}")
        if selectors[condition]:
            selected.add(step["name"])
    return selected


class PostgresMatrixTests(unittest.TestCase):
    COMPATIBILITY = {"Test PostgreSQL catalog compatibility"}
    PRIMARY_PR = {"Test runtime Postgres agent scenarios"}
    PRIMARY_TRUNK = {
        "Test Postgres store (conformance and attempt atomicity)",
        "Test runtime pool-wait binding",
        "Test runtime Postgres agent scenarios",
        "Test cross-backend store differential",
    }

    def test_matrix_has_fixed_explicit_primary_and_compatibility_roles(self) -> None:
        jobs = yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))["jobs"]
        self.assertEqual(
            [
                {"postgres": "14", "role": "compatibility"},
                {"postgres": "16", "role": "primary"},
                {"postgres": "18", "role": "compatibility"},
            ],
            jobs["postgres-store"]["strategy"]["matrix"]["include"],
        )

    def test_event_and_role_selection_runs_the_right_real_tests(self) -> None:
        for event in ("pull_request", "merge_group", "push", "workflow_dispatch"):
            with self.subTest(event=event, role="compatibility"):
                self.assertEqual(
                    self.COMPATIBILITY,
                    selected_postgres_test_steps(event, "compatibility"),
                )
            with self.subTest(event=event, role="primary"):
                expected = (
                    self.PRIMARY_PR
                    if event in ci_plan.DEFERRED_EVENTS
                    else self.PRIMARY_TRUNK
                )
                self.assertEqual(expected, selected_postgres_test_steps(event, "primary"))

    def test_commands_pin_live_catalog_version_and_runtime_identity_oracle(self) -> None:
        job = yaml.safe_load(CI_WORKFLOW.read_text(encoding="utf-8"))["jobs"][
            "postgres-store"
        ]
        steps = {step["name"]: step for step in job["steps"]}
        compatibility = steps["Test PostgreSQL catalog compatibility"]["run"]
        self.assertIn("committed_shape_artifact_matches_the_ddl_artifact", compatibility)
        self.assertIn(
            "a_mismatched_version_stamp_is_reported_without_a_column_diff",
            compatibility,
        )
        self.assertIn(
            "agent_scenario_public_process_parents_are_literal_and_crash_atomic_on_postgres",
            steps["Test runtime Postgres agent scenarios"]["run"],
        )

    def test_postgres_conclusion_fails_closed_for_every_supported_event(self) -> None:
        for event in ("pull_request", "merge_group", "push", "workflow_dispatch"):
            for result in ("skipped", "failure", "cancelled"):
                with self.subTest(event=event, result=result):
                    needs = successful_needs()
                    apply_event_deferrals(needs, event)
                    needs["postgres-store"]["result"] = result
                    problems = ci_plan.evaluate_conclusion(
                        needs, event_name=event, ref="refs/heads/main"
                    )
                    self.assertTrue(any("postgres-store" in problem for problem in problems))
            with self.subTest(event=event, result="missing"):
                needs = successful_needs()
                apply_event_deferrals(needs, event)
                del needs["postgres-store"]
                problems = ci_plan.evaluate_conclusion(
                    needs, event_name=event, ref="refs/heads/main"
                )
                self.assertTrue(any("postgres-store" in problem for problem in problems))


class FuzzSmokeTests(unittest.TestCase):
    def test_fuzz_smoke_is_gated_rust_and_trunk_only(self) -> None:
        self.assertEqual("rust", ci_plan.GATED_JOBS.get("fuzz-smoke"))
        self.assertIn("fuzz-smoke", ci_plan.TRUNK_ONLY_JOBS)

    def test_fuzz_smoke_job_is_bounded_and_off_the_pr_critical_path(self) -> None:
        workflow = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]
        job = workflow["fuzz-smoke"]
        self.assertIn("timeout-minutes", job, "fuzz smoke must carry an explicit timeout")
        self.assertIn("github.event_name != 'pull_request'", job["if"])
        self.assertIn("github.event_name != 'merge_group'", job["if"])
        self.assertIn("fuzz-smoke", workflow["ci-conclusion"]["needs"])

    def test_fuzz_smoke_must_be_skipped_on_deferred_events(self) -> None:
        for event in ("pull_request", "merge_group"):
            needs = successful_needs()
            apply_event_deferrals(needs, event)
            needs["fuzz-smoke"]["result"] = "success"
            problems = ci_plan.evaluate_conclusion(needs, event_name=event)
            self.assertTrue(any("fuzz-smoke" in problem for problem in problems))
            needs["fuzz-smoke"]["result"] = "skipped"
            self.assertEqual([], ci_plan.evaluate_conclusion(needs, event_name=event))

    def test_fuzz_paths_are_known_to_the_classifier(self) -> None:
        plan = ci_plan.classify(
            [
                ("M", "fuzz/fuzz_targets/remote_wire_dto.rs"),
                ("A", "fuzz/corpus/remote_wire_dto/seed-turn-input.json"),
            ]
        )
        self.assertEqual("false", plan["fail_open"])
        self.assertEqual({"true"}, {plan[family] for family in ci_plan.FAMILIES})

    def test_committed_seed_corpus_is_present_and_non_empty(self) -> None:
        import check_fuzz_corpus

        targets = check_fuzz_corpus.fuzz_targets(
            check_fuzz_corpus.FUZZ_MANIFEST.read_text(encoding="utf-8")
        )
        self.assertGreaterEqual(len(targets), 4)
        self.assertEqual(
            [], check_fuzz_corpus.corpus_problems(targets, check_fuzz_corpus.CORPUS_ROOT)
        )

    def test_missing_or_empty_seed_is_detected(self) -> None:
        import tempfile

        import check_fuzz_corpus

        with tempfile.TemporaryDirectory() as tmp:
            corpus_root = Path(tmp)
            problems = check_fuzz_corpus.corpus_problems(["absent"], corpus_root)
            self.assertTrue(any("missing corpus directory" in problem for problem in problems))
            (corpus_root / "hollow").mkdir()
            (corpus_root / "hollow" / "seed-empty").write_bytes(b"")
            problems = check_fuzz_corpus.corpus_problems(["hollow"], corpus_root)
            self.assertTrue(any("empty seed files" in problem for problem in problems))


class WorkflowRegistrationTests(unittest.TestCase):
    def test_every_ci_job_is_registered_or_allowlisted(self) -> None:
        self.assertEqual(set(), unregistered_ci_jobs(CI_WORKFLOW.read_text(encoding="utf-8")))

    def test_only_process_operations_waits_for_worker_artifacts(self):
        jobs = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]
        other = jobs["functional-e2e"]
        consumer = jobs["functional-e2e-process-operations"]
        self.assertEqual("plan", other["needs"])
        self.assertEqual(["plan", "worker-artifacts"], consumer["needs"])
        self.assertEqual(other["if"], consumer["if"])
        self.assertEqual(["process-operations"],
                         [leg["name"] for leg in consumer["strategy"]["matrix"]["include"]])
        self.assertEqual({"agent-service", "agent-workbench", "effect-group-conformance",
                          "workflow-graph-roundtrip", "version-bump-recreation",
                          "session-lease-triage", "slack-clone-full-host"},
                         {leg["name"] for leg in other["strategy"]["matrix"]["include"]})
        self.assertFalse(any("worker binaries" in step.get("name", "") for step in other["steps"]))
        self.assertTrue(any(step.get("name") == "Download worker binaries" for step in consumer["steps"]))

    def test_rogue_job_is_caught(self) -> None:
        workflow_copy = CI_WORKFLOW.read_text(encoding="utf-8").rstrip()
        workflow_copy += "\n\n  rogue-job:\n    runs-on: ubuntu-latest\n"
        self.assertEqual({"rogue-job"}, unregistered_ci_jobs(workflow_copy))


class QueueRequiredCompileLaneTests(unittest.TestCase):
    """The three dedicated compile lanes must witness every queued head.

    FIG-2854. The job IDs, the workflow condition and the docs-only
    expectation are spelled out by hand here rather than derived from
    ``ci_plan.QUEUE_REQUIRED_COMPILE_JOBS``: a test that reads its expectation
    out of the set under test still passes after someone empties the set.
    """

    JOBS = ("lashlang-git-consumer", "package-feature-checks", "runtime-feature-boundary")
    CONDITION = (
        "github.event_name == 'merge_group' || "
        "(github.event_name != 'pull_request' && needs.plan.outputs.rust == 'true')"
    )

    def board(self, event: str, docs_only: bool = False) -> dict:
        needs = successful_needs()
        if docs_only:
            needs["plan"]["outputs"].update(
                {"docs_only": "true", **{family: "false" for family in ci_plan.FAMILIES}}
            )
            for job in ci_plan.GATED_JOBS:
                needs[job]["result"] = "skipped"
            if event in ci_plan.DEFERRED_EVENTS:
                # Pre-existing policy, unchanged here: the PostgreSQL job is
                # already required on both deferred events whatever the plan.
                needs["postgres-store"]["result"] = "success"
        apply_event_deferrals(needs, event)
        return needs

    def test_the_three_lanes_are_registered_and_no_longer_trunk_only(self) -> None:
        aggregator_needs = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]["ci-conclusion"]["needs"]
        for job in self.JOBS:
            with self.subTest(job=job):
                self.assertEqual("rust", ci_plan.GATED_JOBS.get(job))
                self.assertIn(job, ci_plan.QUEUE_REQUIRED_COMPILE_JOBS)
                self.assertNotIn(job, ci_plan.TRUNK_ONLY_JOBS)
                self.assertIn(job, aggregator_needs)
        self.assertEqual(set(self.JOBS), set(ci_plan.QUEUE_REQUIRED_COMPILE_JOBS))

    def test_workflow_conditions_run_them_on_every_merge_group(self) -> None:
        jobs = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]
        for job in self.JOBS:
            with self.subTest(job=job):
                self.assertEqual(self.CONDITION, jobs[job]["if"])

    def test_success_is_required_on_a_production_merge_group(self) -> None:
        self.assertEqual([], ci_plan.evaluate_conclusion(self.board("merge_group"), "merge_group"))
        for job in self.JOBS:
            for result in ("skipped", "failure", "cancelled"):
                with self.subTest(job=job, result=result):
                    needs = self.board("merge_group")
                    needs[job]["result"] = result
                    problems = ci_plan.evaluate_conclusion(needs, "merge_group")
                    self.assertIn(
                        f"queue-required compile job {job} ended with {result!r} on a"
                        " merge_group event, expected success",
                        problems,
                    )

    def test_success_is_required_on_a_docs_only_merge_group(self) -> None:
        needs = self.board("merge_group", docs_only=True)
        self.assertEqual([], ci_plan.evaluate_conclusion(needs, "merge_group"))
        for job in self.JOBS:
            for result in ("skipped", "failure", "cancelled"):
                with self.subTest(job=job, result=result):
                    needs = self.board("merge_group", docs_only=True)
                    needs[job]["result"] = result
                    problems = ci_plan.evaluate_conclusion(needs, "merge_group")
                    self.assertIn(
                        f"queue-required compile job {job} ended with {result!r} on a"
                        " merge_group event, expected success",
                        problems,
                    )

    def test_pull_requests_still_expect_them_skipped(self) -> None:
        self.assertEqual([], ci_plan.evaluate_conclusion(self.board("pull_request"), "pull_request"))
        for job in self.JOBS:
            for result in ("success", "failure", "cancelled"):
                with self.subTest(job=job, result=result):
                    needs = self.board("pull_request")
                    needs[job]["result"] = result
                    self.assertIn(
                        f"queue-required compile job {job} ended with {result!r} on a"
                        " pull_request event, expected skipped",
                        ci_plan.evaluate_conclusion(needs, "pull_request"),
                    )

    def test_push_and_dispatch_keep_the_rust_family_expectation(self) -> None:
        for event in ("push", "workflow_dispatch"):
            needs = self.board(event)
            self.assertEqual([], ci_plan.evaluate_conclusion(needs, event, "refs/heads/main"))
            for job in self.JOBS:
                with self.subTest(event=event, job=job):
                    wrongly_skipped = self.board(event)
                    wrongly_skipped[job]["result"] = "skipped"
                    self.assertIn(
                        f"{job} ended with 'skipped' although plan.rust required it to run",
                        ci_plan.evaluate_conclusion(wrongly_skipped, event, "refs/heads/main"),
                    )
                    # A docs-only trunk run may still skip them: unchanged.
                    docs = self.board(event, docs_only=True)
                    docs[job]["result"] = "skipped"
                    self.assertEqual(
                        [], ci_plan.evaluate_conclusion(docs, event, "refs/heads/main")
                    )

    def test_the_other_deferred_jobs_are_untouched(self) -> None:
        still_deferred = (
            "heavy-tests",
            "stack-budget",
            "confidence-fast",
            "confidence-fast-summary",
            "s3-store",
            "functional-e2e",
            "functional-e2e-process-operations",
            "fuzz-smoke",
        )
        self.assertEqual(set(still_deferred), set(ci_plan.TRUNK_ONLY_JOBS))
        jobs = yaml.safe_load(CI_WORKFLOW.read_text())["jobs"]
        for job in still_deferred:
            with self.subTest(job=job):
                self.assertIn("github.event_name != 'merge_group'", jobs[job]["if"])
                needs = self.board("merge_group")
                needs[job]["result"] = "success"
                self.assertIn(
                    f"trunk-only job {job} ended with 'success' on a merge_group event,"
                    " expected skipped",
                    ci_plan.evaluate_conclusion(needs, "merge_group"),
                )


if __name__ == "__main__":
    unittest.main()
