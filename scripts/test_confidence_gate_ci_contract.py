#!/usr/bin/env python3
from __future__ import annotations

import datetime as dt
import json
import pathlib
import re
import runpy
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
CONFIDENCE_WORKFLOW = ROOT / ".github" / "workflows" / "confidence.yml"
PERF_WORKFLOW = ROOT / ".github" / "workflows" / "perf.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
GATE = ROOT / "scripts" / "confidence-gate.sh"
QUARANTINE_CHECK = ROOT / "scripts" / "check_test_quarantines.py"
CARGO_TOML = ROOT / "Cargo.toml"
JUSTFILE = ROOT / "justfile"
FOCUSED_SQLITE_REPRO = ROOT / "scripts" / "lash-sim-focused-sqlite-repro.sh"
# The two micro lanes (sim unit/oracle + perf-guard identity) share one shard.
FAST_SHARDS = [
    "scenario-harnesses",
    "fault-matrix",
    "sim-unit-perf-guards",
    "sim-generated",
    "minimizer-fixtures",
]
OLD_BROAD_CI_STEP_NAME = "Run bounded broad " + "replay/backend confidence"
OLD_BROAD_CI_JOB_ID = "bounded-" + "broad-replay-backend"
OLD_BROAD_CI_ARTIFACT = "bounded-" + "broad-replay-backend-confidence"
OLD_BROAD_CI_OUT_ROOT = "target/confidence-ci/" + OLD_BROAD_CI_JOB_ID
VALIDATE_QUARANTINE_MANIFEST = runpy.run_path(str(QUARANTINE_CHECK))[
    "validate_manifest"
]


def shell_int_constant(script: str, name: str) -> int:
    match = re.search(rf"^{re.escape(name)}=([0-9]+)$", script, re.MULTILINE)
    if match is None:
        raise AssertionError(f"missing shell constant {name}")
    return int(match.group(1))


def workflow_job_block(workflow: str, job_id: str) -> str:
    marker = f"  {job_id}:\n"
    start = workflow.index(marker)
    next_job = re.search(r"^  [A-Za-z0-9_-]+:\n", workflow[start + len(marker) :], re.MULTILINE)
    if next_job is None:
        return workflow[start:]
    return workflow[start : start + len(marker) + next_job.start()]


class ConfidenceGateCiContractTest(unittest.TestCase):
    def assert_quarantine_fixture_invalid(
        self, payload: dict[str, object], message: str
    ) -> None:
        with tempfile.TemporaryDirectory() as directory:
            fixture = pathlib.Path(directory) / "test-quarantines.json"
            fixture.write_text(json.dumps(payload), encoding="utf-8")
            with self.assertRaisesRegex(ValueError, message):
                VALIDATE_QUARANTINE_MANIFEST(fixture, dt.date(2026, 1, 1))

    def test_ci_shards_fast_confidence_not_broad_replay_backend(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        self.assertIn("confidence-fast:", workflow)
        self.assertIn("confidence-fast-summary:", workflow)
        self.assertIn('bash scripts/confidence-gate.sh "fast:${{ matrix.shard }}"', workflow)
        self.assertIn("bash scripts/confidence-gate.sh fast:summary", workflow)
        self.assertIn(
            "pattern: confidence-fast-*-attempt-${{ github.run_attempt }}", workflow
        )
        self.assertIn(
            "name: confidence-fast-summary-attempt-${{ github.run_attempt }}",
            workflow,
        )
        summary = workflow_job_block(workflow, "confidence-fast-summary")
        self.assertIn("- confidence-fast\n", summary)
        self.assertNotIn("Confidence gate fast lane", workflow)
        self.assertNotIn("bash scripts/confidence-gate.sh fast\n", workflow)
        for shard in FAST_SHARDS:
            self.assertIn(f"- {shard}", workflow)
            self.assertIn(shard, gate)
        self.assertNotIn(OLD_BROAD_CI_JOB_ID, workflow)
        self.assertNotIn("Bounded Broad " + "Replay/Backend", workflow)
        self.assertNotIn(OLD_BROAD_CI_STEP_NAME, workflow)
        self.assertNotIn(OLD_BROAD_CI_ARTIFACT, workflow)
        self.assertNotIn(OLD_BROAD_CI_OUT_ROOT, workflow)

        min_seeds = shell_int_constant(gate, "SIM_SEARCH_MIN_SEEDS")
        min_boundaries = shell_int_constant(gate, "SIM_SEARCH_MIN_MAX_BOUNDARIES")
        self.assertGreaterEqual(min_seeds, 4)
        self.assertGreaterEqual(min_boundaries, 256)

    def test_failure_artifacts_are_attempt_qualified_and_quarantines_are_checked(
        self,
    ) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")
        perf_workflow = PERF_WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        self.assertIn("python3 scripts/check_test_quarantines.py", workflow)
        self.assertIn(
            "confidence-artifacts-attempt-${{ github.run_attempt }}",
            confidence_workflow,
        )
        self.assertIn(
            "confidence-sim-search-${{ matrix.shard }}-attempt-${{ github.run_attempt }}",
            confidence_workflow,
        )
        self.assertIn("if: always()", perf_workflow)
        self.assertIn(
            "perf-guard-full-attempt-${{ github.run_attempt }}", perf_workflow
        )
        self.assertIn(
            '"artifact_name": "confidence-artifacts-attempt-${GITHUB_RUN_ATTEMPT:-local}"',
            gate,
        )

    def test_quarantine_validator_rejects_malformed_expired_and_duplicate_fixtures(
        self,
    ) -> None:
        valid_entry = {
            "id": "FIG-100",
            "test_selector": "crate::tests::flaky",
            "mode": "retry",
            "owner": "@runtime",
            "issue_url": "https://linear.app/example/issue/FIG-100",
            "rca_status": "investigating",
            "expires_on": "2026-02-01",
        }
        self.assert_quarantine_fixture_invalid(
            {
                "schema": "lash.test-quarantines.v1",
                "quarantines": [{key: value for key, value in valid_entry.items() if key != "owner"}],
            },
            "missing fields: owner",
        )
        self.assert_quarantine_fixture_invalid(
            {
                "schema": "lash.test-quarantines.v1",
                "quarantines": [{**valid_entry, "expires_on": "2025-12-31"}],
            },
            "quarantine expired",
        )
        self.assert_quarantine_fixture_invalid(
            {
                "schema": "lash.test-quarantines.v1",
                "quarantines": [valid_entry, valid_entry],
            },
            "duplicate quarantine id",
        )
        self.assert_quarantine_fixture_invalid(
            {
                "schema": "lash.test-quarantines.v1",
                "quarantines": [
                    valid_entry,
                    {**valid_entry, "id": "FIG-101"},
                ],
            },
            "duplicate quarantine target",
        )

    def test_lint_job_runs_clippy_fmt_and_boundary_guards(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        # The server-side lint gate is a first-class CI job.
        self.assertIn("  lint:\n", workflow)

        # The lint job runs the same four checks the local push gate runs, so a
        # green local gate implies a green CI lint job (and vice versa): fmt
        # --check, the `-D warnings` all-targets clippy gate, and the two
        # boundary guards that otherwise gate nothing.
        lint = workflow_job_block(workflow, "lint")
        self.assertIn("cargo fmt --all --check", lint)
        self.assertIn("cargo clippy --workspace --all-targets --locked", lint)
        self.assertIn("-- -D warnings", lint)
        self.assertIn("bash scripts/check-core-ui-boundary.sh", lint)
        self.assertIn("bash scripts/check-workflow-graph-model.sh", lint)
        self.assertIn("bash scripts/check-production-file-size.sh", lint)

    def test_lint_job_checks_and_previews_pr_release_notes(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        lint = workflow_job_block(workflow, "lint")

        self.assertIn("fetch-depth: 0", lint)
        self.assertIn("if: github.event_name == 'pull_request'", lint)
        self.assertIn('git merge-base "origin/${{ github.base_ref }}" HEAD', lint)
        self.assertIn("python3 scripts/release_notes.py check-pr", lint)
        self.assertIn('--summary "$GITHUB_STEP_SUMMARY"', lint)

    def test_workflow_graph_example_is_in_functional_matrix(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        justfile = JUSTFILE.read_text(encoding="utf-8")
        functional = workflow_job_block(workflow, "functional-e2e")

        self.assertIn("workflow-graph-roundtrip", functional)
        self.assertIn("recipe: workflow-graph-integration-verify", functional)
        self.assertIn("uses: actions/setup-node@v6", functional)
        self.assertIn("run: just ${{ matrix.recipe }}", functional)
        self.assertIn("workflow-graph-integration-verify:", justfile)
        self.assertIn(
            "cargo test -p workflow-graph-roundtrip --all-targets --locked",
            justfile,
        )
        self.assertIn("run build", justfile)

    def test_sim_search_lane_is_sharded_and_budgeted_at_plan_targets(self) -> None:
        gate = GATE.read_text(encoding="utf-8")
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")
        workflow = WORKFLOW.read_text(encoding="utf-8")

        required_gate_snippets = [
            "run_sim_search_lane()",
            'sim_search_shard="${requested_lane#sim-search:}"',
            '"schema": "lash.confidence.sim-search-run.v1"',
            'search_seeds="${LASH_SIM_DEFAULT_SEEDS:-256}"',
            'search_max_boundaries="${LASH_SIM_DEFAULT_MAX_BOUNDARIES:-500}"',
            'search_seeds="${LASH_SIM_FULL_SEEDS:-5000}"',
            'search_max_boundaries="${LASH_SIM_FULL_MAX_BOUNDARIES:-2000}"',
            'local search_shard="${LASH_SIM_SHARD:-1/1}"',
            "--mode search",
            '--shard "$search_shard"',
            "sim search lane must run in search mode",
        ]
        for snippet in required_gate_snippets:
            self.assertIn(snippet, gate)

        # The fast lane is the merge gate: its generated sim lane keeps the
        # binary's fast-random defaults and never runs the search lane.
        self.assertIn('if [ "$lane" = "fast" ]; then\n    return\n  fi', gate)
        self.assertNotIn("scheduled-depth", gate)
        self.assertNotIn("BROAD_SCHEDULED_DEPTH", gate)

        # Weekly full confidence partitions one search seed space: shard 1/9 on
        # the main job, shards 2/9..9/9 as matrix jobs.
        required_confidence_snippets = [
            "sim-search:",
            'bash scripts/confidence-gate.sh "sim-search:${{ matrix.shard }}/9"',
            "shard: [2, 3, 4, 5, 6, 7, 8, 9]",
            "LASH_SIM_SHARD",
            "'1/9'",
        ]
        for snippet in required_confidence_snippets:
            self.assertIn(snippet, confidence_workflow)

        # The per-merge CI workflow must not run search
        # shards or override sim budgets.
        self.assertNotIn("sim-search", workflow)
        self.assertNotIn("LASH_SIM_SHARD", workflow)
        self.assertNotIn("LASH_SIM_FULL_SEEDS", workflow)

    def test_fast_gate_has_first_class_shards_and_parallel_minimizers(self) -> None:
        gate = GATE.read_text(encoding="utf-8")

        required_snippets = [
            "fast_shards=(",
            "run_fast_shard()",
            "run_fast_aggregate()",
            "write_fast_matrix_summary()",
            "write_fast_shard_summary()",
            "run_cargo_tests()",
            "cargo nextest run",
            "run_sim_unit_suite()",
            "run_sim_generated_lane()",
            "run_minimizer_fixture_suite()",
            "--skip generated_sim_profile_writes_trace_replay_and_provider_artifacts",
            "--skip minimizer_preserves",
            "--skip minimizer_writes_replayable_regression_package",
            "cargo build -p lash-sim --locked --bin lash-sim",
            "LASH_MINIMIZER_FIXTURE_JOBS",
            'xargs -n 1 -P "$fixture_jobs"',
            '"schema": "lash.confidence.fast-shard-summary.v1"',
            '"sharded": True',
        ]
        for snippet in required_snippets:
            self.assertIn(snippet, gate)

        for shard in FAST_SHARDS:
            self.assertIn(f"fast:{shard}", gate)

    def test_release_is_manual_and_requires_a_green_main_commit(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        required_snippets = [
            "workflow_dispatch:",
            "release_sha:",
            'requested="${REQUESTED_SHA:-origin/main}"',
            'git merge-base --is-ancestor "${sha}" origin/main',
            'gh run list --workflow ci.yml --commit "${sha}"',
            'run.get("event") == "push"',
            'run.get("conclusion") == "success"',
            "release_notes.py collect --require",
            "release_version.py print-next",
            'git tag "${tag}" "${sha}"',
        ]
        for snippet in required_snippets:
            self.assertIn(snippet, workflow)
        self.assertNotIn("\n  push:\n", workflow)

    def test_runtime_release_publishes_sdk_without_host_assets(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        publish = workflow_job_block(workflow, "publish")
        publish_crates = workflow_job_block(workflow, "publish-crates")

        self.assertNotIn("build-release-assets", workflow)
        self.assertNotIn("perf-guard-full", workflow)
        self.assertNotIn("install_lash.sh", workflow)
        self.assertIn("needs: [prepare-release, publish-crates]", publish)
        self.assertIn("needs: [prepare-release, validate-release-ref]", publish_crates)

    def test_broad_lane_is_manual_or_scheduled_confidence_not_ci_cd(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        self.assertIn("- fast", confidence_workflow)
        self.assertIn("- default", confidence_workflow)
        self.assertIn("- broad", confidence_workflow)
        self.assertIn("- full", confidence_workflow)
        self.assertIn("run: bash scripts/confidence-gate.sh", confidence_workflow)
        self.assertIn("inputs.lane || 'full'", confidence_workflow)
        self.assertIn("schedule:", confidence_workflow)
        self.assertNotIn("bash scripts/confidence-gate.sh broad", workflow)

        self.assertIn('"bounded_broad_confidence": {', gate)
        self.assertIn('"workflow": "Confidence"', gate)
        self.assertIn('"lane": "broad"', gate)
        self.assertIn('"trigger": "workflow_dispatch_or_schedule"', gate)
        self.assertIn(
            '"artifact_name": "confidence-artifacts-attempt-${GITHUB_RUN_ATTEMPT:-local}"',
            gate,
        )
        self.assertIn('"full_confidence_claim": "false"', gate)

    def test_full_lane_artifact_contract_requires_true_full_evidence(self) -> None:
        gate = GATE.read_text(encoding="utf-8")

        required_snippets = [
            'if [ "$lane" = "full" ] && [ "$mutation_scope" != "full" ]; then',
            'if [ "$lane" = "full" ] && [ "$coverage_scope" != "run" ]; then',
            "full_mutation_suites_complete()",
            "mutation_evidence_status()",
            "coverage_evidence_status()",
            "restate_postgres_workers_e2e_status()",
            '"artifact_contract": {',
            '"schema": "lash.confidence.summary-artifact-contract.v1"',
            '"full_lane": {',
            '"confidence_class": "true_full"',
            '"required_coverage_scope": "run"',
            '"effective_coverage_scope": "${coverage_scope}"',
            '"coverage_evidence_status": "$(coverage_evidence_status)"',
            '"required_mutation_scope": "full"',
            '"effective_mutation_scope": "${mutation_scope}"',
            '"mutation_evidence": "$(mutation_evidence_path)"',
            '"mutation_evidence_status": "$(mutation_evidence_status)"',
            '"full_mutation_status": "$(full_mutation_status)"',
            '"required_restate_postgres_workers_e2e": "sim/restate-postgres-workers-e2e.json"',
            '"restate_postgres_workers_e2e_status": "$(restate_postgres_workers_e2e_status)"',
            "run_restate_postgres_workers_e2e",
            '"status": "not_run"',
            '"reason": "distributed Restate/Postgres/MinIO worker e2e is full-lane-only"',
        ]

        for snippet in required_snippets:
            self.assertIn(snippet, gate)

    def test_focused_sqlite_seed_tail_repro_gate_is_named_and_exact(self) -> None:
        gate = GATE.read_text(encoding="utf-8")
        repro = FOCUSED_SQLITE_REPRO.read_text(encoding="utf-8")

        required_gate_snippets = [
            "run_focused_sqlite_seed_tail_repro()",
            'step "Focused generated SQLite seed-tail repro"',
            'scripts/lash-sim-focused-sqlite-repro.sh "$repro_dir"',
            "run_focused_sqlite_seed_tail_repro",
            '"focused_sqlite_seed_tail_repro": "$([ -f "${out_dir}/sim/focused-sqlite-seed-tail/focused-sqlite-seed-tail.json" ]',
        ]
        for snippet in required_gate_snippets:
            self.assertIn(snippet, gate)

        required_repro_snippets = [
            '"schema": "lash.confidence.focused-sqlite-seed-tail-repro.v1"',
            'focused_single_seed="4101155038242989457"',
            'focused_tail_previous_seed="17785827714152183977"',
            '--profile "$profile"',
            '--max-boundaries "$max_boundaries"',
            'run_case "single-seed-4101155038242989457" "$focused_single_seed"',
            '"tail-seeds-17785827714152183977-4101155038242989457"',
            '"sqlite_divergence_reports"',
        ]
        for snippet in required_repro_snippets:
            self.assertIn(snippet, repro)

    def test_static_replay_matrix_does_not_claim_generated_backend_static_replay(self) -> None:
        gate = GATE.read_text(encoding="utf-8")

        required_snippets = [
            'step "Static replay evidence matrix"',
            "generated_static_backend_skip_reason=",
            "generated_backend_fixture_static_skip_reason=",
            "generated_backend_regression_fixture",
            '"schema": "lash.confidence.static-replay-evidence-matrix.v1"',
            "backend equivalence is proved by per-seed generated workload rerun artifacts",
        ]
        for snippet in required_snippets:
            self.assertIn(snippet, gate)

        self.assertNotIn("backend_replayable_regression", gate)
        self.assertNotIn(
            "Every generated trace and every backend-replayable regression trace is replayed through model, SQLite, and Postgres",
            gate,
        )

    def test_generated_postgres_dynamic_rerun_is_bounded_and_artifacted(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        required_snippets = [
            "run_generated_postgres_dynamic_replay()",
            'step "Generated Postgres dynamic backend rerun"',
            "cargo run -p lash-sim --locked -- run-postgres",
            '--seed "$seed"',
            'LASH_POSTGRES_GENERATED_PROFILE:-full-random',
            'LASH_POSTGRES_GENERATED_MAX_BOUNDARIES:-128',
            '"confidence_lane": "generated_dynamic_postgres_backend_rerun"',
            '"generated_postgres_dynamic_replay": "$([ -f "${out_dir}/sim/postgres-generated-rerun/summary.json" ]',
        ]
        for snippet in required_snippets:
            self.assertIn(snippet, gate)

        self.assertIn("- broad", confidence_workflow)
        self.assertIn("inputs.lane || 'full'", confidence_workflow)
        self.assertNotIn("LASH_POSTGRES_GENERATED_PROFILE", workflow)
        self.assertNotIn("LASH_POSTGRES_GENERATED_MAX_BOUNDARIES", workflow)

    def test_property_and_await_cancel_evidence_pinned_in_fast_gate(self) -> None:
        gate = GATE.read_text(encoding="utf-8")

        # The SSE framing property suites (transport plus the Anthropic/Google
        # provider parsers) are pinned as first-class fast-lane evidence in the
        # fault-matrix shard, alongside the existing state-machine/lashlang
        # property runners.
        required_snippets = [
            'step "LLM transport SSE framing property suite"',
            "run_cargo_tests -p lash-llm-transport --locked --test property",
            "run_cargo_tests -p lash-provider-anthropic --locked --test property",
            "run_cargo_tests -p lash-provider-google --locked --test property",
            # Durable-wait session-cancel evidence: the inline effect-host
            # conformance test that exercises
            # effect_host_await_event_session_cancel_resolves_outstanding_waits.
            'step "Inline effect-host await-event session-cancel conformance"',
            "run_cargo_tests -p lash-core --locked inline_effect_host_satisfies_conformance",
        ]
        for snippet in required_snippets:
            self.assertIn(snippet, gate)

    def test_publish_time_version_injection_has_only_post_release_docs_commit(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        release = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        cargo = CARGO_TOML.read_text(encoding="utf-8")

        # The manifest bump commit and pass-1/pass-2 re-run chain are gone. A
        # green main push only validates; a manual release stamps an ephemeral
        # checkout, then updates only the checked-in docs pin after publishing.
        self.assertNotIn("release_version.py set", workflow)
        self.assertNotIn("Commit release version", workflow)
        self.assertNotIn("Dispatch validation pass", workflow)
        self.assertNotIn("Sync release version to staging", workflow)
        self.assertNotIn("gh workflow run ci.yml", workflow)
        # ci.yml carries a bare workflow_dispatch trigger so a maintainer can
        # re-run trunk validation when GitHub drops a push event (it happened
        # to merge commit d70ce7ea: zero check suites were created). This is
        # NOT the old pass-2 bump-commit revalidation chain — the assertions
        # above and below keep that chain dead; the dispatch trigger must stay
        # input-less and must never be invoked from a workflow.
        self.assertIn("workflow_dispatch:", workflow)
        self.assertNotIn("prepare-release:", workflow)
        self.assertIn("workflow_dispatch:", release)

        # main carries the honest dev placeholder; the channel is the source of
        # truth for which release series a cut belongs to.
        self.assertIn('version = "0.0.0-dev"', cargo)
        self.assertIn("[workspace.metadata.release]", cargo)
        self.assertNotIn("0.1.0-alpha.", cargo)

        # The publisher stamps the ephemeral checkout before packaging crates.
        # Host-application binary stamping belongs to lash-cli's release.
        self.assertIn("publish_workspace.py --version", release)
        self.assertIn('release_version.py stamp-docs "${version}"', release)
        self.assertIn("git push origin HEAD:main", release)
        self.assertIn("git rebase origin/main", release)
        self.assertIn("continue-on-error: true", release)
        self.assertIn(
            "Release-Notes: Internal: Stamp documentation version pins", release
        )

    def test_release_notes_are_gated_only_when_a_manual_release_is_cut(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        release = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertNotIn("release-notes-gate", workflow)
        self.assertNotIn("release_notes.py collect --require", workflow)
        prepare_release = workflow_job_block(release, "prepare-release")
        self.assertIn("release_notes.py collect --require", prepare_release)

    def test_workspace_tests_are_sharded_off_the_critical_path(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        # The monolithic `test:` job is gone; a doctest/build-cache writer plus
        # the nextest partition shards replace it.
        self.assertNotIn("  test:\n", workflow)
        self.assertIn("  test-doc:\n", workflow)
        self.assertIn("  test-shard:\n", workflow)
        self.assertIn("--partition count:${{ matrix.shard }}/3", workflow)
        # --no-fail-fast so one failure never hides the rest (alpha.82 lesson).
        self.assertIn("--no-fail-fast", workflow)

    def test_heavy_compile_jobs_route_through_sccache(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        release = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        for job_id in ("test-doc", "test-shard", "confidence-fast", "linux-release-cache"):
            block = workflow_job_block(workflow, job_id)
            self.assertIn("./.github/actions/setup-sccache", block)
        self.assertNotIn("cargo build", release)

    def test_ci_has_no_staging_or_automatic_release_path(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        self.assertNotIn("staging", workflow)
        self.assertNotIn("prepare-release", workflow)
        self.assertNotIn("git tag", workflow)
        self.assertNotIn("gh workflow run release.yml", workflow)


if __name__ == "__main__":
    unittest.main()
