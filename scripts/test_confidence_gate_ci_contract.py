#!/usr/bin/env python3
from __future__ import annotations

import datetime as dt
import json
import os
import pathlib
import re
import runpy
import subprocess
import tempfile
import unittest


ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
CONFIDENCE_WORKFLOW = ROOT / ".github" / "workflows" / "confidence.yml"
PERF_WORKFLOW = ROOT / ".github" / "workflows" / "perf.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
RELEASE_NOTES = ROOT / "scripts" / "release_notes.py"
GATE = ROOT / "scripts" / "confidence-gate.sh"
PUSH_GATE = ROOT / "scripts" / "push-gate.sh"
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
IS_AUTOMATED_DOCS_STAMP = runpy.run_path(str(RELEASE_NOTES))[
    "is_automated_docs_stamp"
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


def shell_function_body(script: str, function_name: str) -> str:
    start_match = re.search(
        rf"^{re.escape(function_name)}\(\) \{{\n", script, re.MULTILINE
    )
    if start_match is None:
        raise AssertionError(f"missing shell function {function_name}")
    next_function = re.search(
        r"^[a-zA-Z_][a-zA-Z0-9_]*\(\) \{\n",
        script[start_match.end() :],
        re.MULTILINE,
    )
    if next_function is None:
        return script[start_match.end() :]
    return script[start_match.end() : start_match.end() + next_function.start()]


def shell_logical_commands(script: str) -> list[str]:
    commands: list[str] = []
    current = ""
    for line in script.splitlines():
        stripped = line.strip()
        current = f"{current} {stripped}".strip()
        if current.endswith("\\"):
            current = current[:-1].rstrip()
            continue
        if current:
            commands.append(current)
        current = ""
    if current:
        commands.append(current)
    return commands


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

    def test_ci_confidence_out_root_matches_every_workflow_artifact_path(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")
        expected_env = (
            "LASH_CONFIDENCE_OUT_DIR: "
            "${{ github.workspace }}/target/confidence"
        )
        self.assertIn(expected_env, workflow)
        self.assertIn(expected_env, confidence_workflow)

        ci_root = ROOT / "target" / "confidence"
        ci_env = {
            **os.environ,
            "CI": "true",
            "GITHUB_ACTIONS": "true",
            "GITHUB_WORKSPACE": str(ROOT),
            "LASH_CONFIDENCE_OUT_DIR": str(ci_root),
            "LASH_CONFIDENCE_MUTATION_SCOPE": "full",
            "LASH_CONFIDENCE_COVERAGE_SCOPE": "run",
        }

        def computed_artifact_dir(selector: str) -> pathlib.Path:
            result = subprocess.run(
                ["bash", str(GATE), "--dry-run", selector],
                cwd=ROOT,
                env=ci_env,
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(result.returncode, 0, result.stderr)
            match = re.search(r"^Artifacts: (.+)$", result.stdout, re.MULTILINE)
            self.assertIsNotNone(match, result.stdout)
            return pathlib.Path(match.group(1))

        computed = {
            f"fast:{shard}": computed_artifact_dir(f"fast:{shard}")
            for shard in FAST_SHARDS
        }
        computed["fast:summary"] = computed_artifact_dir("fast:summary")
        computed["full"] = computed_artifact_dir("full")
        computed["sim-search:2/9"] = computed_artifact_dir("sim-search:2/9")

        for shard in FAST_SHARDS:
            self.assertEqual(
                computed[f"fast:{shard}"], ci_root / "fast" / shard
            )
        self.assertEqual(computed["fast:summary"], ci_root / "fast")
        self.assertEqual(computed["full"], ci_root / "full")
        self.assertEqual(
            computed["sim-search:2/9"], ci_root / "sim-search" / "2-of-9"
        )

        # Pair each workflow-consumed artifact path with the selector output it
        # is meant to consume. This deliberately parses only `path:` values:
        # command-local staging paths are not gate outputs.
        upload_steps = [
            step
            for source in (workflow, confidence_workflow)
            for step in re.split(
                r"(?=^      - (?:name|uses):)", source, flags=re.MULTILINE
            )
            if "uses: actions/upload-artifact@" in step
        ]
        consumed_paths = [
            match.group(1)
            for step in upload_steps
            for match in re.finditer(
                r"^\s+path:\s+(target/confidence.+)$", step, re.MULTILINE
            )
        ]
        computed_relative = {
            selector: path.relative_to(ROOT).as_posix()
            for selector, path in computed.items()
        }
        expected_consumed_paths = {
            computed_relative["fast:summary"],
            computed_relative["fast:scenario-harnesses"].replace(
                "scenario-harnesses", "${{ matrix.shard }}"
            ),
            str(pathlib.PurePosixPath(computed_relative["full"]).parent / "**"),
            str(
                pathlib.PurePosixPath(computed_relative["sim-search:2/9"]).parent
                / "**"
            ),
        }
        self.assertCountEqual(consumed_paths, expected_consumed_paths)

        self.assertIn("path: target/confidence/fast/${{ matrix.shard }}", workflow)
        self.assertIn("path: target/confidence/fast", workflow)
        self.assertIn("path: target/confidence/**", confidence_workflow)
        self.assertIn(
            "path: target/confidence/sim-search/**", confidence_workflow
        )

    def test_store_properties_have_reproducible_pr_and_soak_budgets(self) -> None:
        gate = GATE.read_text(encoding="utf-8")
        justfile = JUSTFILE.read_text(encoding="utf-8")
        scenario_harnesses = shell_function_body(gate, "run_scenario_harnesses")

        self.assertIn("default_store_contract_cases=32", scenario_harnesses)
        self.assertIn('[ "$lane" = "full" ]', scenario_harnesses)
        self.assertIn("default_store_contract_cases=256", scenario_harnesses)
        self.assertEqual(
            scenario_harnesses.count(
                'LASH_STORE_CONTRACT_PROPTEST_CASES="$store_contract_cases"'
            ),
            3,
        )
        self.assertIn("store-contract-soak cases='256':", justfile)
        self.assertIn("default_runtime_persistence_cases=32", scenario_harnesses)
        self.assertIn("default_runtime_persistence_cases=256", scenario_harnesses)
        self.assertEqual(
            scenario_harnesses.count(
                'LASH_RUNTIME_PERSISTENCE_PROPTEST_CASES="$runtime_persistence_cases"'
            ),
            3,
        )
        self.assertIn("runtime-persistence-soak cases='256':", justfile)
        cross_backend_soak = shell_function_body(gate, "run_cross_backend_store_soak")
        self.assertIn('LASH_CROSS_BACKEND_SOAK_CASES:-64', cross_backend_soak)
        self.assertIn('LASH_CROSS_BACKEND_CASES="$cases"', cross_backend_soak)
        self.assertIn("cross-backend-store-soak cases='64' seed='852':", justfile)

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

        # The lint job runs the same checks the local push gate runs, so a
        # green local gate implies a green CI lint job (and vice versa): fmt
        # --check, the `-D warnings` all-targets clippy gate, and the boundary
        # guards that otherwise gate nothing.
        lint = workflow_job_block(workflow, "lint")
        self.assertIn("cargo fmt --all --check", lint)
        self.assertIn("cargo clippy --workspace --all-targets --locked", lint)
        self.assertIn("-- -D warnings", lint)
        self.assertIn("python3 scripts/check-restate-handler-panics.py", lint)
        self.assertIn("bash scripts/check-core-ui-boundary.sh", lint)
        self.assertIn("bash scripts/check-workflow-graph-model.sh", lint)
        self.assertIn("bash scripts/check-production-file-size.sh", lint)
        self.assertIn("python3 scripts/check-transcript-diff.py", lint)
        self.assertLess(
            lint.index("cargo clippy --workspace --all-targets --locked"),
            lint.index("python3 scripts/check-transcript-diff.py"),
        )

    def test_lint_job_checks_and_previews_pr_release_notes(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        lint = workflow_job_block(workflow, "lint")

        self.assertIn("fetch-depth: 0", lint)
        self.assertIn("if: github.event_name == 'pull_request'", lint)
        self.assertIn('git merge-base "origin/${{ github.base_ref }}" HEAD', lint)
        self.assertIn("python3 scripts/release_notes.py check-pr", lint)
        self.assertIn('--summary "$GITHUB_STEP_SUMMARY"', lint)

    def test_push_gate_checks_current_branch_release_notes(self) -> None:
        push_gate = PUSH_GATE.read_text(encoding="utf-8")
        check = shell_function_body(push_gate, "check_current_branch_release_notes")

        self.assertIn('git merge-base "$base_ref" HEAD', check)
        self.assertIn("python3 scripts/release_notes.py check-pr", check)
        self.assertIn('--range "${merge_base}..HEAD"', check)

    def test_push_gate_serializes_live_differential_before_postgres_free_suite(
        self,
    ) -> None:
        push_gate = PUSH_GATE.read_text(encoding="utf-8")
        postgres = shell_function_body(push_gate, "run_postgres_conformance")
        workspace = shell_function_body(push_gate, "run_workspace_tests")

        self.assertIn("--test cross_backend_store_differential", postgres)
        self.assertIn("LASH_REQUIRE_POSTGRES=1", postgres)
        self.assertIn(
            "env -u LASH_POSTGRES_DATABASE_URL -u LASH_REQUIRE_POSTGRES",
            workspace,
        )

    def test_lint_job_runs_database_free_budgeted_perf_smoke(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        lint = workflow_job_block(workflow, "lint")

        self.assertIn("runs-on: blacksmith-8vcpu-ubuntu-2404", lint)
        # PR-time smoke enforces the machine-independent inventory only;
        # duration/allocation ceilings are calibrated on the release profile
        # and enforced by --enforce-budgets in perf.yml and the Release job.
        self.assertIn(
            "profile_runtime.py --profile quick "
            "--enforce-inventory --out .benchmarks/perf-smoke/runtime.json",
            lint,
        )
        self.assertNotIn("--profile quick --enforce-budgets", lint)
        self.assertIn(
            "profile_lashlang.py --debug --iterations 10 "
            "--profile-iterations 10 --out .benchmarks/perf-smoke/lashlang.json",
            lint,
        )
        smoke = lint[lint.index("- name: Run performance harness smoke") :]
        smoke = smoke[: smoke.index("- name: Check core/UI boundary")]
        self.assertNotIn("--scenario all", smoke)
        self.assertIn(
            "rustfmt --edition 2024 --check "
            "crates/lash-perf/src/runtime_perf/measurement/store_hardening.rs",
            lint,
        )

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
            'run.get("event") in ("push", "workflow_dispatch")',
            "run = matching[0]",
            'run.get("conclusion") != "success"',
            "release refused: target ",
            "run.get('databaseId')",
            "run.get('url', 'URL unavailable')",
            "release_notes.py collect --require",
            "release_version.py print-next",
            'git tag "${RELEASE_TAG}" "${RELEASE_SHA}"',
        ]
        for snippet in required_snippets:
            self.assertIn(snippet, workflow)
        self.assertNotIn("\n  push:\n", workflow)
        self.assertLess(
            workflow.index(
                "profile_runtime.py --profile full --release --scenario all "
                "--enforce-budgets"
            ),
            workflow.index('git tag "${RELEASE_TAG}" "${RELEASE_SHA}"'),
        )

    def test_runtime_release_publishes_sdk_without_host_assets(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        publish = workflow_job_block(workflow, "publish")
        publish_crates = workflow_job_block(workflow, "publish-crates")
        validate_release = workflow_job_block(workflow, "validate-release-ref")

        self.assertNotIn("build-release-assets", workflow)
        self.assertNotIn("install_lash.sh", workflow)
        self.assertIn("needs: [prepare-release, publish-crates]", publish)
        self.assertIn("needs: [prepare-release, validate-release-ref]", publish_crates)
        self.assertIn("runs-on: blacksmith-16vcpu-ubuntu-2404", validate_release)
        self.assertIn(
            "ref: ${{ needs.prepare-release.outputs.release_sha }}", validate_release
        )
        self.assertIn(
            "ref: ${{ needs.prepare-release.outputs.release_sha }}", publish_crates
        )
        self.assertIn("ref: ${{ needs.prepare-release.outputs.release_sha }}", publish)
        self.assertIn('head_sha="$(git rev-parse HEAD)"', publish_crates)
        self.assertIn('head_sha="$(git rev-parse HEAD)"', publish)
        self.assertIn(
            "profile_runtime.py --profile full --release --scenario all "
            "--enforce-budgets",
            validate_release,
        )
        self.assertIn(
            "profile_lashlang.py --iterations 2500 --profile-iterations 2500 "
            "--enforce-budgets",
            validate_release,
        )

    def test_full_perf_is_release_gated_and_only_manually_dispatchable(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        perf = PERF_WORKFLOW.read_text(encoding="utf-8")
        release = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        release_cache = workflow_job_block(workflow, "linux-release-cache")

        self.assertIn("workflow_dispatch:", perf)
        self.assertNotIn("schedule:", perf)
        self.assertIn("runs-on: blacksmith-16vcpu-ubuntu-2404", perf)
        self.assertIn("useblacksmith/rust-cache@v3.0.1", release_cache)
        self.assertIn("cargo build --locked --release --workspace", release_cache)
        self.assertNotIn("--target x86_64-unknown-linux-gnu", release_cache)
        for command in (
            "profile_runtime.py --profile full --release --scenario all "
            "--enforce-budgets",
            "profile_lashlang.py --iterations 2500 --profile-iterations 2500 "
            "--enforce-budgets",
        ):
            self.assertIn(command, perf)
            self.assertIn(command, release)

        for workflow_with_postgres in (perf, release):
            self.assertIn("image: postgres:16-alpine", workflow_with_postgres)
            self.assertIn("LASH_POSTGRES_DATABASE_URL:", workflow_with_postgres)

    def test_all_confidence_fast_shards_use_blacksmith(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        confidence_fast = workflow_job_block(workflow, "confidence-fast")

        self.assertIn(
            "- shard: sim-unit-perf-guards\n"
            "            runner: blacksmith-16vcpu-ubuntu-2404",
            confidence_fast,
        )
        self.assertNotIn("ubuntu-latest", confidence_fast)
        self.assertNotIn("Restore cargo cache (GitHub)", confidence_fast)

    def test_broad_lane_is_manual_or_scheduled_confidence_not_ci_cd(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        self.assertIn('type: string', confidence_workflow)
        self.assertNotIn('type: choice', confidence_workflow)
        self.assertNotIn('options:', confidence_workflow)
        self.assertIn('default: "full"', confidence_workflow)
        self.assertIn('CONFIDENCE_SELECTOR: ${{', confidence_workflow)
        self.assertIn(
            'run: bash scripts/confidence-gate.sh "$CONFIDENCE_SELECTOR"',
            confidence_workflow,
        )
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

    def test_confidence_selector_vocabulary_and_area_plans_are_executable(self) -> None:
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        for snippet in (
            "fast+area:store",
            "full+area:effect-host",
            "fast:fault-matrix+area:trigger",
            "sim-search:<i>/<n>",
            "store, process, trigger, effect-host, protocol, provider, sim",
        ):
            self.assertIn(snippet, gate)
        self.assertIn("full+area:<surface>", confidence_workflow)
        self.assertIn("fast:<shard>+area:<surface>", confidence_workflow)

        with tempfile.TemporaryDirectory() as directory:
            env = {"LASH_CONFIDENCE_OUT_DIR": directory}
            store_plan = subprocess.run(
                ["bash", str(GATE), "--dry-run", "fast+area:store"],
                cwd=ROOT,
                env={**os.environ, **env},
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(store_plan.returncode, 0, store_plan.stderr)
            self.assertIn("Area: store", store_plan.stdout)
            self.assertIn("store contracts", store_plan.stdout)
            self.assertNotIn("runtime persistence", store_plan.stdout)
            self.assertEqual(list(pathlib.Path(directory).iterdir()), [])

            shard_plan = subprocess.run(
                [
                    "bash",
                    str(GATE),
                    "sim-search:3/9+area:sim",
                    "--dry-run",
                ],
                cwd=ROOT,
                env={**os.environ, **env},
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(shard_plan.returncode, 0, shard_plan.stderr)
            self.assertIn("search shard 3/9", shard_plan.stdout)

            trigger_plan = subprocess.run(
                ["bash", str(GATE), "--dry-run", "full+area:trigger"],
                cwd=ROOT,
                env={**os.environ, **env},
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(trigger_plan.returncode, 0, trigger_plan.stderr)
            self.assertIn("source filters:", trigger_plan.stdout)
            self.assertIn("crates/lash-core/src/triggers", trigger_plan.stdout)

            invalid = subprocess.run(
                ["bash", str(GATE), "--dry-run", "fast+area:unknown"],
                cwd=ROOT,
                env={**os.environ, **env},
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(invalid.returncode, 2)
            self.assertIn(
                "Areas: store, process, trigger, effect-host, protocol, provider, sim",
                invalid.stderr,
            )

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
            'full:all) echo "true_full"',
            'full:*) echo "area_scoped_full"',
            '"global_full_confidence_claim":',
            '"required_coverage_scope": "run"',
            '"effective_coverage_scope": "${coverage_scope}"',
            '"coverage_evidence_status": "$(coverage_evidence_status)"',
            '"required_mutation_scope": "full"',
            '"effective_mutation_scope": "${mutation_scope}"',
            '"mutation_evidence": "$(mutation_evidence_path)"',
            '"mutation_evidence_status": "$(mutation_evidence_status)"',
            '"full_mutation_status": "$(full_mutation_status)"',
            '"required_restate_postgres_workers_e2e": "$(area_selected process',
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

    def test_model_replay_artifact_does_not_claim_backend_equivalence(self) -> None:
        gate = GATE.read_text(encoding="utf-8")

        required_snippets = [
            'step "Model replay evidence"',
            "run_model_replay_suite()",
            'replay_dir="${out_dir}/sim/model-replay"',
            "generated_backend_regression_fixture",
            '"schema": "lash.confidence.model-replay-evidence.v1"',
            "Backend equivalence is not claimed by this artifact",
        ]
        for snippet in required_snippets:
            self.assertIn(snippet, gate)

        self.assertNotIn("run_cross_backend_replay_suite", gate)
        self.assertNotIn("sim/cross-backend-replay", gate)
        replay_command = shell_function_body(gate, "run_model_replay_command")
        row_format = re.search(
            r"""printf\s+'(?P<json>\{.*?\})\\n'""", replay_command, re.DOTALL
        )
        self.assertIsNotNone(row_format)
        row_keys = set(
            re.findall(r'"([^"]+)"\s*:', row_format.group("json"))
        )
        self.assertNotIn("backend", row_keys)
        self.assertNotIn("skip_reason", row_keys)
        self.assertNotIn("backend_replayable_regression", gate)
        self.assertNotIn(
            "Every generated trace and every backend-replayable regression trace is replayed through model, SQLite, and Postgres",
            gate,
        )

    def test_model_replay_empty_corpus_writes_failed_verdict_and_exits_nonzero(
        self,
    ) -> None:
        gate = GATE.read_text(encoding="utf-8")
        replay_suite = shell_function_body(gate, "run_model_replay_suite")
        harness = f"""\
set -euo pipefail
out_dir="$1"
step() {{ :; }}
run_model_replay_suite() {{
{replay_suite}
run_model_replay_suite
"""
        with tempfile.TemporaryDirectory() as directory:
            completed = subprocess.run(
                ["bash", "-c", harness, "model-replay-contract", directory],
                check=False,
                capture_output=True,
                text=True,
            )
            summary_path = (
                pathlib.Path(directory) / "sim" / "model-replay" / "summary.json"
            )
            self.assertEqual(
                completed.returncode,
                1,
                f"empty replay corpus did not fail:\n{completed.stdout}\n{completed.stderr}",
            )
            summary = json.loads(summary_path.read_text(encoding="utf-8"))
            self.assertEqual(summary["status"], "failed")
            self.assertEqual(summary["row_count"], 0)

    def test_seeded_mutation_failure_writes_failed_verdict_and_exits_nonzero(
        self,
    ) -> None:
        gate = GATE.read_text(encoding="utf-8")
        recorded = shell_function_body(gate, "run_mutants_recorded")
        finalize = shell_function_body(gate, "finalize_mutation_gate")
        harness = f"""\
set -euo pipefail
out_dir="$1"
mutation_scope="smoke"
mutation_failures=0
write_mutation_evidence_summary() {{ :; }}
write_confidence_summary() {{ printf '%s\\n' "$1" >"${{out_dir}}/summary-verdict"; }}
run_mutants_recorded() {{
{recorded}
finalize_mutation_gate() {{
{finalize}
seeded_mutation_failure() {{
  if [ "${{CARGO_TARGET_DIR:-}}" != "target" ]; then
    return 3
  fi
  return 2
}}
export CARGO_TARGET_DIR="deliberately-shared-target"
run_mutants_recorded \
  "seeded survivor" \
  "${{out_dir}}/seeded-mutant" \
  seeded_mutation_failure
finalize_mutation_gate
"""
        with tempfile.TemporaryDirectory() as directory:
            completed = subprocess.run(
                ["bash", "-c", harness, "mutation-contract", directory],
                check=False,
                capture_output=True,
                text=True,
            )
            artifact = pathlib.Path(directory) / "seeded-mutant"
            command_status = json.loads(
                (artifact / "confidence-status.json").read_text(encoding="utf-8")
            )
            self.assertEqual(command_status["status"], "failed")
            self.assertEqual(command_status["exit_code"], 2)
            self.assertEqual(
                completed.returncode,
                1,
                f"seeded mutation failure did not fail:\n{completed.stdout}\n{completed.stderr}",
            )
            self.assertEqual(
                (pathlib.Path(directory) / "summary-verdict").read_text(
                    encoding="utf-8"
                ),
                "failed\n",
            )

    def test_mutation_failure_is_aggregated_after_full_lane_evidence(self) -> None:
        gate = GATE.read_text(encoding="utf-8")
        main = gate[gate.rindex("\nrun_scenario_harnesses\n") :]

        smoke = main.index("run_mutation_smoke")
        broad_postgres = main.index("run_broad_postgres_evidence")
        conformance = main.index("run_postgres_conformance")
        workers_e2e = main.index("run_restate_postgres_workers_e2e")
        full_mutation = main.index("run_mutation_full")
        aggregate = main.index("finalize_mutation_gate")

        self.assertLess(smoke, broad_postgres)
        self.assertLess(broad_postgres, conformance)
        self.assertLess(conformance, workers_e2e)
        self.assertLess(workers_e2e, full_mutation)
        self.assertLess(full_mutation, aggregate)
        self.assertEqual(main.count("finalize_mutation_gate"), 1)
        self.assertIn("if ! finalize_mutation_gate; then\n    exit 1\n  fi", main)

    def test_durable_stores_are_critical_coverage_and_mutation_packages(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        critical_packages = gate.split("critical_packages=(", 1)[1].split(")", 1)[0]
        self.assertIn("lash-sqlite-store", critical_packages)
        self.assertIn("lash-postgres-store", critical_packages)
        for function_name in ("run_mutation_smoke", "run_mutation_full"):
            body = shell_function_body(gate, function_name)
            loop_headers = re.findall(
                r"^\s*for\s+package\s+in\s+(.+);\s*do\s*$", body, re.MULTILINE
            )
            self.assertEqual(['"${selected_packages[@]}"'], loop_headers)
            self.assertIn('if [ "$package" = "lash-postgres-store" ]; then', body)
            self.assertIn("run_postgres_mutants_recorded", body)

        postgres_mutation = shell_function_body(
            gate, "run_postgres_mutants_recorded"
        )
        self.assertIn('start_mutation_postgres "$artifact"', postgres_mutation)
        self.assertIn(
            'LASH_POSTGRES_DATABASE_URL="$mutation_postgres_database_url"',
            postgres_mutation,
        )
        self.assertIn("LASH_REQUIRE_POSTGRES=1", postgres_mutation)
        self.assertIn('"$@" --jobs "$mutation_jobs"', postgres_mutation)

        derive_jobs = shell_function_body(gate, "derive_mutation_jobs")
        harness = f"""\
set -euo pipefail
out_dir="$(mktemp -d)"
derive_mutation_jobs() {{
{derive_jobs}
[ "$(derive_mutation_jobs 1)" = 1 ]
[ "$(derive_mutation_jobs 4)" = 2 ]
[ "$(derive_mutation_jobs 8)" = 4 ]
[ "$(derive_mutation_jobs 32)" = 4 ]
"""
        completed = subprocess.run(
            ["bash", "-c", harness],
            check=False,
            capture_output=True,
            text=True,
        )
        self.assertEqual(
            completed.returncode,
            0,
            f"machine-derived mutation job contract failed:\n"
            f"{completed.stdout}\n{completed.stderr}",
        )
        self.assertEqual(gate.count('local jobs="${LASH_MUTATION_JOBS:-2}"'), 0)

        coverage_body = shell_function_body(gate, "run_coverage_blind_spots")
        coverage_loops = re.findall(
            r"^\s*for\s+package\s+in\s+(.+);\s*do\s*$",
            coverage_body,
            re.MULTILINE,
        )
        self.assertEqual(['"${selected_packages[@]}"'], coverage_loops)
        self.assertIn('coverage_package_args+=(-p "$package")', coverage_body)
        self.assertIn(
            '''critical_package_regex="$(IFS='|'; printf '%s' "${selected_packages[*]}")"''',
            coverage_body,
        )
        self.assertIn(
            'awk -v critical_package_regex="$critical_package_regex"',
            coverage_body,
        )
        self.assertIn(
            'file ~ ("/crates/(" critical_package_regex ")/")',
            coverage_body,
        )
        self.assertIn('if [ "$lane" = "full" ]; then', gate)
        self.assertIn('cron: "29 4 * * 0"', confidence_workflow)
        self.assertNotIn("scripts/confidence-gate.sh default", workflow)

    def test_postgres_ci_lane_requires_database_configuration(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")
        push_gate = PUSH_GATE.read_text(encoding="utf-8")
        postgres_store_job = workflow_job_block(workflow, "postgres-store")

        self.assertIn('LASH_REQUIRE_POSTGRES: "1"', workflow)
        self.assertIn('LASH_CROSS_BACKEND_CASES: "4"', postgres_store_job)
        self.assertIn(
            'LASH_CROSS_BACKEND_CASES="${LASH_CROSS_BACKEND_PR_CASES:-4}"',
            push_gate,
        )
        conformance_calls = [
            command
            for command in shell_logical_commands(gate)
            if re.search(
                r"\bcargo test -p lash-postgres-store\b.*(?:^|\s)--test\s+conformance(?:\s|$)",
                command,
            )
        ]
        self.assertGreater(len(conformance_calls), 0)
        self.assertEqual(
            len(conformance_calls),
            sum(
                "LASH_REQUIRE_POSTGRES=1" in command
                for command in conformance_calls
            ),
        )

    def test_minio_ci_lane_requires_storage_configuration(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        s3_store_job = workflow_job_block(workflow, "s3-store")

        self.assertIn(
            "run: cargo test -p lash-s3-store --locked", s3_store_job
        )
        self.assertIn(
            "LASH_MINIO_ENDPOINT: http://127.0.0.1:9000", s3_store_job
        )
        self.assertIn('LASH_REQUIRE_MINIO: "1"', s3_store_job)
        self.assertIn("attachment_blob_store_differential_agrees", s3_store_job)

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

        self.assertIn('type: string', confidence_workflow)
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

    def test_provider_conformance_is_explicitly_featured_in_ci(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        test_doc = workflow_job_block(workflow, "test-doc")

        for provider in ("openai", "anthropic", "google"):
            self.assertIn(
                f"cargo test -p lash-provider-{provider} --features testing --locked conformance",
                test_doc,
            )

    def test_lash_runtime_default_tests_are_pinned_to_the_feature_boundary_lane(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        test_doc = workflow_job_block(workflow, "test-doc")
        push_gate = PUSH_GATE.read_text(encoding="utf-8")
        feature_boundary = shell_function_body(
            push_gate, "run_runtime_feature_boundary_check"
        )
        command = "cargo test -p lash-runtime --no-default-features --locked"
        count_command = (
            "count=$(cargo test -p lash-runtime --no-default-features --locked "
            "--lib -- --list | grep -c ': test$')"
        )
        count_floor = (
            '[ "$count" -ge 130 ] || { echo '
            '"default-build lash-runtime tests regressed: $count"; exit 1; }'
        )

        for snippet in (command, count_command, count_floor):
            self.assertIn(snippet, test_doc)
            self.assertIn(snippet, feature_boundary)

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
        self.assertIn(
            "Skipping docs pin for superseded ${RELEASE_TAG}", release
        )
        self.assertIn(
            "git tag --list 'v*' --sort=-v:refname", release
        )
        self.assertIn("python3 scripts/lint_docs.py", release)
        self.assertIn("gh workflow run ci.yml --ref main", release)
        self.assertIn("permissions:\n  contents: read\n  actions: read", release)
        publish = workflow_job_block(release, "publish")
        self.assertIn("permissions:\n      contents: write\n      actions: write", publish)

    def test_release_notes_are_gated_only_when_a_manual_release_is_cut(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        release = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        self.assertNotIn("release-notes-gate", workflow)
        self.assertNotIn("release_notes.py collect --require", workflow)
        prepare_release = workflow_job_block(release, "prepare-release")
        self.assertIn("release_notes.py collect --require", prepare_release)

    def test_automated_docs_stamp_cannot_satisfy_next_release_notes_gate(self) -> None:
        release_notes = RELEASE_NOTES.read_text(encoding="utf-8")

        self.assertTrue(IS_AUTOMATED_DOCS_STAMP("docs: stamp release 0.1.0-alpha.113"))
        self.assertFalse(IS_AUTOMATED_DOCS_STAMP("docs: explain release 0.1.0-alpha.113"))
        self.assertIn("if is_automated_docs_stamp(subject):", release_notes)

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

        for job_id in (
            "test-doc",
            "test-shard",
            "lint",
            "confidence-fast",
            "linux-release-cache",
        ):
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
