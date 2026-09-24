#!/usr/bin/env python3
from __future__ import annotations

import ast
import datetime as dt
import functools
import json
import os
import pathlib
import re
import runpy
import subprocess
import tempfile
import tomllib
import unittest

import yaml


ROOT = pathlib.Path(__file__).resolve().parent.parent
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
CHECKOUT_STEP = ROOT / "scripts" / "ci" / "checkout-step.sh"
CONFIDENCE_WORKFLOW = ROOT / ".github" / "workflows" / "confidence.yml"
PERF_WORKFLOW = ROOT / ".github" / "workflows" / "perf.yml"
RELEASE_WORKFLOW = ROOT / ".github" / "workflows" / "release.yml"
RELEASE_CACHE_WORKFLOW = ROOT / ".github" / "workflows" / "release-cache.yml"
SEAL_CACHE_WORKFLOW = ROOT / ".github" / "workflows" / "seal-cache.yml"
MOLD_RUSTFLAGS = "-C link-arg=-fuse-ld=mold"
GATE = ROOT / "scripts" / "confidence-gate.sh"
PUSH_GATE = ROOT / "scripts" / "push-gate.sh"
STORE_TESTS = ROOT / "scripts" / "ci" / "store-tests.sh"
FEATURE_COVERAGE = ROOT / "scripts" / "feature-coverage.toml"
GENERATOR = ROOT / "tools" / "bazel" / "generate_build_files.py"
LANE_TABLE = ROOT / "tools" / "bazel" / "feature_lanes.bzl"


def feature_lane_table() -> dict[str, list[str]]:
    """The generated lane -> Bazel label table; a Starlark dict is a Python one."""
    source = LANE_TABLE.read_text(encoding="utf-8")
    marker = "FEATURE_LANES = "
    return ast.literal_eval(source[source.index(marker) + len(marker) :].strip())
PRE_COMMIT_CONFIG = ROOT / ".pre-commit-config.yaml"
QUARANTINE_CHECK = ROOT / "scripts" / "check_test_quarantines.py"
PERF_SCENARIOS_RS = ROOT / "crates" / "lash-perf" / "src" / "runtime_perf" / "scenarios.rs"
PERF_PHASE_PROBE_RS = (
    ROOT
    / "crates"
    / "lash-perf"
    / "src"
    / "runtime_perf"
    / "measurement"
    / "phase_probe.rs"
)
CARGO_TOML = ROOT / "Cargo.toml"
JUSTFILE = ROOT / "justfile"
# The two micro lanes (sim unit/oracle + perf-guard identity) share one shard.
FAST_SHARDS = [
    "scenario-harnesses",
    "fault-matrix",
    "sim-unit-perf-guards",
    "sim-generated",
    "minimizer-fixtures",
]
VALIDATE_QUARANTINE_MANIFEST = runpy.run_path(str(QUARANTINE_CHECK))[
    "validate_manifest"
]



@functools.lru_cache(maxsize=None)
def _store_tests_stub_bin() -> str:
    """A PATH entry whose `bazel`, `cargo` and `python3` echo their argv instead of running."""
    directory = pathlib.Path(tempfile.mkdtemp(prefix="store-tests-stub-"))
    for tool in ("bazel", "cargo", "python3"):
        stub = directory / tool
        stub.write_text(
            f'#!/usr/bin/env bash\nprintf "%s\\n" "{tool} $*"\n', encoding="utf-8"
        )
        stub.chmod(0o755)
    return str(directory)


@functools.lru_cache(maxsize=None)
def store_suite_branches(suite: str) -> tuple[str, str]:
    """Returns the (Bazel, Cargo) commands one `store-tests.sh` suite renders.

    The service jobs dispatch to that script rather than inlining a command, so
    a pin on a command or a test name has to follow the name into the branch
    that actually runs it -- and has to hold on BOTH branches, because an
    untrusted event (fork or Dependabot PR) gets no cache credentials and takes
    the Cargo half.

    This runs the script with `bazel` and `cargo` stubbed to echo their argv,
    rather than splitting the arm's text on `else`. Six of the suites are now
    rendered from one table instead of written twice, so there is no `else` to
    split on -- and reading what the script actually invokes is the stronger
    check for the three shaped arms too: `pg-store` and `s3-store` expand a
    generated label file that text-splitting could only see as `labels s3`.
    """
    rendered = []
    for trusted in ("true", "false"):
        environment = {
            key: value
            for key, value in os.environ.items()
            if key != "GITHUB_ACTIONS"
        }
        environment["PATH"] = (
            f"{_store_tests_stub_bin()}{os.pathsep}{environment['PATH']}"
        )
        environment["BAZEL_TRUSTED"] = trusted
        # `scripts/ci/with-service.sh` exports the slot count to every suite
        # it wraps; the sharded PostgreSQL suite refuses to run without it.
        environment["LASH_POSTGRES_SLOT_COUNT"] = "4"
        result = subprocess.run(
            ["bash", str(STORE_TESTS), suite],
            cwd=ROOT,
            env=environment,
            capture_output=True,
            text=True,
            check=True,
        )
        rendered.append(result.stdout)
    return rendered[0], rendered[1]


def store_suite_for_step(step: str) -> str:
    """Reads the suite name out of a `run: bash scripts/ci/store-tests.sh <s>`."""
    match = re.search(r"scripts/ci/store-tests\.sh\s+(\S+)", step)
    if match is None:
        raise AssertionError(f"step does not dispatch to store-tests.sh:\n{step}")
    return match.group(1)


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


def workflow_step_block(job_block: str, step_name: str) -> str:
    marker = f"      - name: {step_name}\n"
    start = job_block.index(marker)
    next_step = re.search(
        r"^      - name: ", job_block[start + len(marker) :], re.MULTILINE
    )
    if next_step is None:
        return job_block[start:]
    return job_block[start : start + len(marker) + next_step.start()]


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


def shell_function_definition(script: str, function_name: str) -> str:
    match = re.search(
        rf"^{re.escape(function_name)}\(\) \{{\n.*?^\}}\n",
        script,
        re.MULTILINE | re.DOTALL,
    )
    if match is None:
        raise AssertionError(f"missing shell function {function_name}")
    return match.group(0)


def shell_assoc_array(script: str, name: str) -> dict[str, str]:
    """Parses `declare -A <name>=( [key]="value" ... )` out of a shell script."""
    marker = f"declare -A {name}=(\n"
    if marker not in script:
        raise AssertionError(f"missing shell associative array {name}")
    body = script.split(marker, 1)[1].split("\n)\n", 1)[0]
    entries = re.findall(r'^\s*\[([^\]]+)\]="([^"]*)"$', body, re.MULTILINE)
    parsed = dict(entries)
    if len(parsed) != len(entries):
        raise AssertionError(f"{name} declares a key twice")
    return parsed


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
    def test_full_stage_jobs_share_exactly_one_build_artifact(self):
        jobs = yaml.safe_load(CONFIDENCE_WORKFLOW.read_text())["jobs"]
        consumers = {"confidence-harnesses", "confidence-generated", "confidence-minimizer",
                     "confidence-backends", "confidence-coverage",
                     "confidence-mutation-core", "confidence-mutation-sim",
                     "confidence-mutation-authority", "confidence-mutation-packages", "sim-search"}
        self.assertEqual(consumers | {"confidence", "confidence-build", "confidence-conclusion"}, set(jobs))
        artifact = "confidence-build-${{ github.sha }}-${{ github.run_attempt }}"
        producer = jobs["confidence-build"]
        uploads = [s for j in jobs.values() for s in j["steps"]
                   if "upload-artifact@" in s.get("uses", "") and s["with"]["name"] == artifact]
        self.assertEqual(1, len(uploads))
        self.assertEqual("error", uploads[0]["with"]["if-no-files-found"])
        self.assertIn("tar -cf", str(producer["steps"]))
        for job in consumers:
            with self.subTest(job=job):
                self.assertEqual("confidence-build", jobs[job]["needs"])
                downloads = [s for s in jobs[job]["steps"] if "download-artifact@" in s.get("uses", "")]
                self.assertEqual([artifact], [s["with"]["name"] for s in downloads])
                self.assertIn("tar -xmf", str(jobs[job]["steps"]))
                self.assertNotIn("rust-cache@", str(jobs[job]["steps"]))
                self.assertNotIn("continue-on-error", jobs[job])
        for job in jobs.values():
            self.assertGreater(job["timeout-minutes"], 0)
            self.assertLess(job["timeout-minutes"], 360)
        for job in ("confidence-generated", "sim-search"):
            self.assertEqual(list(range(1, 10)), jobs[job]["strategy"]["matrix"]["shard"])
            self.assertIs(False, jobs[job]["strategy"]["fail-fast"])
        self.assertIn("inputs.lane != 'full'", jobs["confidence"]["if"])
        self.assertEqual("always()", jobs["confidence-conclusion"]["if"])
        self.assertIn("scripts/ci/conclusion.py conclusion", str(jobs["confidence-conclusion"]["steps"]))

    def test_full_stage_partition_calls_every_original_function_once(self):
        functions = {
            "harnesses": ["run_scenario_harnesses", "run_state_machine_and_fault_matrix", "run_sim_unit_suite",
                          "write_provider_transport_exclusion_evidence", "write_sim_lane_declarations",
                          "write_full_lane_prerequisites", "write_postgres_effect_history_status",
                          "write_restate_postgres_workers_e2e_lane_status"],
            "generated": ["run_sim_generated_lane"],
            "minimizer": ["run_minimizer_fixture_suite", "run_focused_sqlite_seed_tail_repro"],
            "backends": ["run_local_backend_conformance", "run_backend_contention_evidence",
                         "run_current_postgres_trace_replay_evidence", "run_postgres_conformance"],
            "workers": ["run_restate_postgres_workers_e2e"],
            "coverage": ["run_coverage_blind_spots"],
            "mutation-core": ["run_lash_core_direct_model_mutation_evidence",
                              "run_process_lease_preimage_mutation_evidence"],
            "mutation-sim": ["run_lash_sim_runtime_completion_mutation_evidence"],
            "mutation-authority": ["run_authority_rebind_mutation_evidence"],
            "mutation-packages": ["run_mutation_smoke", "run_mutation_full", "finalize_mutation_gate"],
        }
        all_functions = {f for fs in functions.values() for f in fs}
        stubs = "\n".join(f"{f}() {{ echo {f}; }}" for f in all_functions)
        stage_script = ROOT / "scripts/ci/confidence-stage.sh"
        with tempfile.TemporaryDirectory() as tmp:
            for stage, expected in functions.items():
                env = dict(os.environ, LASH_CONFIDENCE_STAGE=stage, LASH_CONFIDENCE_PACKAGE="lash-internal-core")
                shell = stubs + '\nassert_no_panics_in_artifacts() { :; }\nrequested_selector=full\nmutation_commands_run=1\nmutation_failures=0\nout_dir="$1"\nsource "$2"'
                result = subprocess.run(["bash", "-euc", shell, "stage-test", tmp, str(stage_script)],
                                        env=env, capture_output=True, text=True)
                self.assertEqual(0, result.returncode, result.stderr)
                self.assertEqual(expected, result.stdout.splitlines(), stage)
        generated = shell_function_definition(GATE.read_text(), "run_sim_generated_lane")
        self.assertIn('cmd+=(--shard "${LASH_SIM_SHARD:?generated stage requires a shard}")', generated)
        self.assertIn('if [ "${LASH_CONFIDENCE_STAGE:-}" != "generated" ]; then', generated)
        self.assertIn('source "$repo/scripts/ci/confidence-stage.sh"', GATE.read_text())

    def test_generated_stage_partitions_full_budget_without_search(self):
        function = shell_function_definition(GATE.read_text(), "run_sim_generated_lane")
        shell = 'step() { :; }\ncargo() { printf "%s\\n" "$@"; }\nrun_sim_search_lane() { echo SEARCH; }\n' + function + '\nlane=full\nout_dir=/tmp/evidence\nrun_sim_generated_lane'
        for shard in range(1, 10):
            env = {k: v for k, v in os.environ.items() if not k.startswith("LASH_SIM_")}
            env.update(LASH_CONFIDENCE_STAGE="generated", LASH_SIM_SHARD=f"{shard}/9")
            result = subprocess.run(["bash", "-euc", shell], env=env, capture_output=True, text=True)
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertEqual(["run", "-p", "lash-sim", "--locked", "--", "run", "--out", "/tmp/evidence/sim",
                              "--profile", "full-random", "--shard", f"{shard}/9"], result.stdout.splitlines())

    def test_confidence_checkout_uses_trigger_sha_in_a_shallow_repository(self):
        jobs = yaml.safe_load(CONFIDENCE_WORKFLOW.read_text())["jobs"]
        scripts = {next(s["run"] for s in job["steps"] if s["name"] == "Check out repository")
                   for job in jobs.values()}
        # Every producer, consumer and conclusion uses the identical checkout:
        # the canonical step scripts/test_checkout_step.py holds every workflow to.
        self.assertEqual(1, len(scripts))
        script = scripts.pop()
        self.assertEqual(CHECKOUT_STEP.read_text(), script)
        self.assertIn('git config gc.auto 0', script)
        self.assertIn('git checkout --detach --force FETCH_HEAD', script)
        with tempfile.TemporaryDirectory() as tmp:
            root = pathlib.Path(tmp)
            remote = root / "repo.git"
            remote.mkdir()
            def git(*args):
                return subprocess.check_output(["git", "-C", str(remote), *args], text=True).strip()
            git("init", "-q")
            git("config", "user.name", "Samuel Galanakis")
            git("config", "user.email", "47306720+SamGalanakis@users.noreply.github.com")
            git("-c", "core.hooksPath=/dev/null", "commit", "--allow-empty", "-qm", "Add fixture")
            sha = git("rev-parse", "HEAD")
            git("-c", "core.hooksPath=/dev/null", "commit", "--allow-empty", "-qm", "Advance fixture")
            clone = root / "checkout"
            clone.mkdir()
            env = dict(os.environ, GITHUB_SERVER_URL=root.as_uri(), GITHUB_REPOSITORY="repo",
                       GITHUB_SHA=sha, CHECKOUT_TOKEN="fixture-token")
            result = subprocess.run(["bash", "-euc", script], cwd=clone, env=env, capture_output=True, text=True)
            self.assertEqual(0, result.returncode, result.stderr)
            head = subprocess.check_output(["git", "-C", str(clone), "rev-parse", "HEAD"], text=True).strip()
            self.assertEqual(sha, head)
            self.assertTrue((clone / ".git/shallow").exists())

    def test_worker_profiles_retain_workspace_outputs_without_changing_segments(self):
        jobs = yaml.safe_load(WORKFLOW.read_text())["jobs"]
        producer = jobs["worker-artifacts"]
        workers = jobs["restate-postgres-workers"]
        for job, key in ((producer, "linux-worker-release"), (workers, "linux-worker-tests")):
            cache = next(s["with"] for s in job["steps"] if "rust-cache@" in s.get("uses", ""))
            self.assertEqual(key, cache["shared-key"])
            self.assertIs(True, cache["cache-workspace-crates"])
            self.assertNotEqual(False, cache["save-if"])
        self.assertEqual([1, 2], workers["strategy"]["matrix"]["segment"])
        self.assertIn("worker-artifacts", workers["needs"])
        self.assertIn("cargo build --locked --release -p lash-restate-postgres-workers-e2e --bins",
                      next(s["run"] for s in producer["steps"] if s["name"] == "Build worker binaries once"))
        self.assertIn("LASH_E2E_PREBUILT_BIN_DIR", str(workers["steps"]))

    def test_seal_cache_writer_mirrors_the_seal_lane(self) -> None:
        # seal-cache.yml is the only main-scoped writer of the seal lane's
        # rust-cache key. rust-cache builds that key from `shared-key`, the
        # `workspaces` target dir, the toolchain and every CARGO*/RUST* env
        # var, so the writer and the reader must agree on all of them or the
        # lane restores nothing and pays the cold trybuild build. The
        # generation suffix is the only way to retire a tree whose shape
        # changed under an unchanged lockfile: rust-cache never re-saves a
        # key it restored in full.
        ci = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        writer = yaml.safe_load(SEAL_CACHE_WORKFLOW.read_text(encoding="utf-8"))
        lane = next(
            s for s in ci["jobs"]["check"]["steps"]
            if "rust-cache@" in s.get("uses", "")
        )
        (writer_job,) = writer["jobs"].values()
        saver = next(s for s in writer_job["steps"] if "rust-cache@" in s.get("uses", ""))
        self.assertEqual(lane["uses"], saver["uses"])
        # The lane restores only: a save from a pull request lands in that
        # PR's own branch scope, which nothing else can read.
        self.assertIs(False, lane["with"]["save-if"])
        restore = {key: value for key, value in lane["with"].items() if key != "save-if"}
        self.assertEqual(restore, saver["with"])
        self.assertRegex(saver["with"]["shared-key"], r"^linux-seal-[0-9]+$")
        self.assertNotIn("save-if", saver["with"])
        self.assertEqual("${{ github.workspace }}/target-seal", ci["jobs"]["check"]["env"]["CARGO_TARGET_DIR"])
        self.assertEqual(ci["jobs"]["check"]["env"]["CARGO_TARGET_DIR"], writer_job["env"]["CARGO_TARGET_DIR"])
        for name in ("CARGO_TERM_COLOR", "LASH_CI_FEATURES", "RUSTFLAGS"):
            self.assertEqual(ci["env"][name], writer["env"][name], name)

    def test_confidence_schedule_matrix_declared_artifacts_have_writers(self) -> None:
        gate = GATE.read_text(encoding="utf-8")
        marker = "confidence_schedule_table=(\n"
        self.assertIn(marker, gate, "confidence schedule table is missing")
        table_body = gate.split(marker, 1)[1].split("\n)\n", 1)[0]
        rows = [
            ast.literal_eval(line.strip())
            for line in table_body.splitlines()
            if line.strip().startswith('"')
        ]
        self.assertTrue(rows, "confidence schedule table is empty")

        expected_selectors = {
            "fast:all",
            *(f"fast:{shard}" for shard in FAST_SHARDS),
            "fast:summary",
            "default",
            "broad",
            "full",
            "sim-search",
        }
        self.assertEqual(
            expected_selectors,
            {row.split("|", 1)[0] for row in rows},
        )
        areas = ("store", "process", "trigger", "effect-host", "protocol", "provider", "sim")
        matrix = {
            (row.split("|", 4)[0], row.split("|", 4)[1])
            for row in rows
        }
        for selector in expected_selectors:
            for area in areas:
                if (selector, area) not in matrix:
                    continue
                self.assertTrue(
                    any(
                        row.split("|", 4)[0] == selector
                        and row.split("|", 4)[1] == area
                        for row in rows
                    ),
                    (selector, area),
                )

        # The key -> path map is the gate's, not a second copy: a row names a
        # key and nothing else, so two rows cannot disagree about a path.
        artifact_paths = shell_assoc_array(gate, "confidence_artifact_paths")
        self.assertEqual(17, len(artifact_paths), artifact_paths)
        for key, path in artifact_paths.items():
            self.assertRegex(key, r"^[a-z0-9_]+$")
            self.assertRegex(path, r"^[a-z0-9./-]+\.json$")
        # The keys the gate actually resolves at runtime. With the paths gone
        # from the writers, a key is only reachable through one of the three
        # schedule accessors or the fast matrix summary's shard-owned list.
        declaration_keys = set(
            re.findall(
                r"(?:scheduled_existing_artifact_path|scheduled_artifact_path|"
                r'schedule_has_artifact|artifact_path) "?([a-z0-9_]+)"?',
                gate,
            )
        ) | set(re.findall(r'^\s+"([a-z0-9_]+):[a-z-]+"$', gate, re.MULTILINE))
        declaration_keys -= {"key"}
        self.assertLessEqual(declaration_keys, set(artifact_paths), declaration_keys)
        scheduled_keys = {
            key
            for row in rows
            for key in filter(None, row.split("|", 4)[4].split(","))
        }
        writer_text = "\n".join(
            shell_function_body(gate, function)
            for function in (
                "write_sim_lane_declarations",
                "write_fast_shard_summary",
                "write_confidence_summary",
            )
        )
        for key in artifact_paths:
            if f'"{key}":' in writer_text:
                self.assertIn(key, scheduled_keys, key)
        for raw_row in rows:
            fields = raw_row.split("|", 4)
            self.assertEqual(5, len(fields), raw_row)
            selector, area, suite, description, raw_artifacts = fields
            self.assertTrue(area, raw_row)
            self.assertTrue(suite, raw_row)
            self.assertTrue(description, raw_row)
            for key in filter(None, raw_artifacts.split(",")):
                # A row carries keys only. The path is not representable here,
                # so the 146 declarations cannot disagree about 17 paths.
                self.assertNotIn("=", key, raw_row)
                self.assertIn(key, artifact_paths, raw_row)
                self.assertIn(key, declaration_keys, key)

        # Every declared path is reachable only through the map: no summary
        # writer or schedule row may spell one out again.
        for key, path in artifact_paths.items():
            self.assertEqual(1, gate.count(f'"{path}"'), path)

    def test_area_scoping_filters_execution_predicates_like_the_plan(self) -> None:
        gate = GATE.read_text(encoding="utf-8")
        marker = "confidence_schedule_table=(\n"
        table_body = gate.split(marker, 1)[1].split("\n)\n", 1)[0]
        rows = [
            ast.literal_eval(line.strip())
            for line in table_body.splitlines()
            if line.strip().startswith('"')
        ]
        areas = ("store", "process", "trigger", "effect-host", "protocol", "provider", "sim")
        cases = (
            ("default+area:store", "default", "all", "store"),
            ("broad+area:store", "broad", "all", "store"),
            ("fast:fault-matrix+area:trigger", "fast", "fault-matrix", "trigger"),
            ("fast:sim-generated", "fast", "sim-generated", "all"),
        )

        schedule_selector = shell_function_definition(gate, "schedule_selector")
        schedule_row_matches_area = shell_function_definition(
            gate, "schedule_row_matches_area"
        )
        schedule_has_area = shell_function_definition(gate, "schedule_has_area")
        area_selected = shell_function_definition(gate, "area_selected")

        for selector, lane, fast_shard, requested_area in cases:
            effective_selector = (
                f"fast:{fast_shard}" if lane == "fast" else lane
            )
            expected = {
                candidate: any(
                    row.split("|", 4)[0] == effective_selector
                    and row.split("|", 4)[1] == candidate
                    and (
                        requested_area == "all"
                        or row.split("|", 4)[1] == requested_area
                    )
                    for row in rows
                )
                for candidate in areas
            }
            harness = "\n".join(
                (
                    "set -eu",
                    f'lane={lane!r}',
                    f'fast_shard={fast_shard!r}',
                    'sim_search_shard=""',
                    f'area={requested_area!r}',
                    marker.rstrip("\\n"),
                    table_body,
                    ")",
                    schedule_selector,
                    schedule_row_matches_area,
                    schedule_has_area,
                    area_selected,
                    "for candidate in " + " ".join(areas) + "; do",
                    '  if area_selected "$candidate"; then printf "%s\\n" "$candidate"; fi',
                    "done",
                )
            )
            completed = subprocess.run(
                ["bash", "-c", harness],
                check=False,
                capture_output=True,
                text=True,
            )
            self.assertEqual(completed.returncode, 0, completed.stderr)
            actual = set(filter(None, completed.stdout.splitlines()))
            self.assertEqual(
                {candidate for candidate, selected in expected.items() if selected},
                actual,
                f"execution area predicate drifted for {selector}",
            )

    def test_heavy_suites_are_split_between_ci_and_heavy_profiles(self) -> None:
        """The ordinary/heavy split lives in nextest profiles, not job scripts.

        `profile.ci` (the workspace test job) must exclude the heavy suites its comment
        promises, `profile.ci-heavy` (the trunk-only heavy-tests job) must run
        them, and each job must name its profile — otherwise a rename on one
        side silently drops the suites from CI entirely.
        """
        workflow = WORKFLOW.read_text(encoding="utf-8")
        nextest = (ROOT / ".config" / "nextest.toml").read_text(encoding="utf-8")

        workspace_tests = workflow_job_block(workflow, "workspace-tests")
        # The workspace job runs only for untrusted events; its Cargo profile
        # and workspace breadth remain the same.
        self.assertIn("cargo nextest run --profile ci --locked", workspace_tests)
        self.assertIn('nextest_args=(--workspace -E "${skip}")', workspace_tests)
        heavy = workflow_job_block(workflow, "heavy-tests")
        self.assertIn("--profile ci-heavy --workspace", heavy)

        self.assertIn("[profile.ci-heavy]", nextest)
        heavy_filter = (
            "durable_fault_matrix_real_cargo_filters_chunk_"
        )
        ci_profile = nextest.split("[profile.ci]", 1)[1].split("[profile.ci-heavy]", 1)[0]
        ci_heavy_profile = nextest.split("[profile.ci-heavy]", 1)[1]
        self.assertIn("default-filter", ci_profile)
        self.assertIn(heavy_filter, ci_profile)
        self.assertIn("default-filter", ci_heavy_profile)
        self.assertIn(heavy_filter, ci_heavy_profile)
        # The trybuild ui binary leaves the workspace job without a heavy-side run;
        # its gate is check's seal step.
        self.assertIn("binary(ui)", ci_profile)
        self.assertNotIn("binary(ui)", ci_heavy_profile)
        self.assertIn("--test ui", workflow_job_block(workflow, "check"))

    def test_dispatch_only_jobs_defer_on_pr_and_merge_group_events(self) -> None:
        """Heavy suites run on manual dispatch only: 2026-08-25 ruling, tightened.

        There is no automatic trunk run any more — ci.yml has no `push`
        trigger — so `workflow_dispatch` is the sole home of the heavy
        families, and it is the profile release.yml certifies. Reassess after
        the FIG-2169 test-prune sweep. The job-level guard, the ci_plan
        accounting set, and the conclusion's deferral behavior must agree, or a
        PR either re-pays the heavy suites or merges green while a heavy job
        silently never runs.
        """
        workflow = WORKFLOW.read_text(encoding="utf-8")
        dispatch_only = {
            "heavy-tests",
            "stack-budget",
            "s3-store",
            "functional-e2e",
            "functional-e2e-process-operations",
            "fuzz-smoke",
            # Deferred Unicode and the lashlang consumer left the
            # PR/merge-group board entirely: the queue runs the same minimal
            # board as a pull request, and the release dispatch is their sole
            # home. Feature lanes came back to every trusted event (FIG-3572).
            "unicode-tests",
            "lashlang-git-consumer",
        }
        guard = "github.event_name == 'workflow_dispatch'"
        for job in sorted(dispatch_only):
            block = workflow_job_block(workflow, job)
            self.assertIn("workflow_dispatch", block, job)
            self.assertIn(guard, block, job)
        # No `push` trigger at all, and no job may resurrect one.
        self.assertNotIn("\n  push:\n", workflow)
        self.assertNotIn("'push'", workflow)

        plan = runpy.run_path(str(ROOT / "scripts" / "ci_plan.py"))
        self.assertEqual(plan["DISPATCH_ONLY_JOBS"], dispatch_only)
        self.assertEqual(plan["DEFERRED_EVENTS"], {"pull_request", "merge_group"})

        evaluate = plan["evaluate_conclusion"]
        needs = {
            job: {"result": "success", "outputs": {}}
            for job in plan["UNGATED_JOBS"]
            | set(plan["GATED_JOBS"])
            | plan["BAZEL_TEST_JOBS"]
        }
        needs["plan"]["outputs"] = dict.fromkeys(plan["FAMILIES"], "true") | {
            "docs_only": "false",
            "fail_open": "false",
        }
        # This scenario exercises deferral, so it is the unselected case: a
        # trusted pull request that does select `restate_suites` runs the live
        # Restate legs instead (asserted below and in test_ci_plan.py).
        needs["plan"]["outputs"]["restate_suites"] = "false"
        needs["workspace-tests"]["result"] = "skipped"
        # Trusted events seal the API inside `bazel-tests`.
        needs["check"]["result"] = "skipped"
        for job in dispatch_only:
            needs[job] = {"result": "skipped", "outputs": {}}
        needs["bazel-tests-tail"] = {"result": "skipped", "outputs": {}}
        self.assertEqual(evaluate(needs, "pull_request"), [])
        needs["bazel-tests-tail"] = {"result": "success", "outputs": {}}
        self.assertEqual(evaluate(needs, "merge_group"), [])
        # The exception to the dispatch-only ruling: on a trusted pull request
        # whose diff selects `restate_suites`, the functional-e2e Restate legs
        # run and must succeed — skipping them is a conclusion failure. The
        # merge group still defers them.
        restate_needs = {
            job: {**value, "outputs": dict(value.get("outputs", {}))}
            for job, value in needs.items()
        }
        restate_needs["plan"]["outputs"] = dict(needs["plan"]["outputs"])
        restate_needs["plan"]["outputs"]["restate_suites"] = "true"
        self.assertEqual(evaluate(restate_needs, "merge_group"), [])
        restate_needs["bazel-tests-tail"]["result"] = "skipped"
        restate_needs["functional-e2e"]["result"] = "success"
        restate_needs["functional-e2e-process-operations"]["result"] = "success"
        self.assertEqual(evaluate(restate_needs, "pull_request"), [])
        restate_needs["functional-e2e"]["result"] = "skipped"
        self.assertEqual(
            evaluate(restate_needs, "pull_request"),
            [
                "dispatch-only job functional-e2e ended with 'skipped' on a"
                " pull_request event, expected success"
            ],
        )
        dispatch_needs = {
            job: {"result": "success", "outputs": dict(value.get("outputs", {}))}
            for job, value in needs.items()
        }
        dispatch_needs["plan"]["outputs"] = dict(needs["plan"]["outputs"])
        dispatch_needs["workspace-tests"]["result"] = "skipped"
        dispatch_needs["check"]["result"] = "skipped"
        self.assertEqual(evaluate(dispatch_needs, "workflow_dispatch"), [])
        for job in ("worker-artifacts", "restate-postgres-workers", "restate-postgres-workers-summary"):
            needs[job] = {"result": "skipped", "outputs": {}}
        self.assertIn(
            "ungated job restate-postgres-workers ended with 'skipped', expected success",
            plan["evaluate_conclusion"](needs, "pull_request"),
        )

        # Workers E2E runs on the full-profile dispatch and on pull requests
        # carrying the `ci:workers` label, and nowhere else.
        workers_guard = (
            "github.event_name == 'workflow_dispatch'\n"
            "      || (github.event_name == 'pull_request'"
            " && contains(github.event.pull_request.labels.*.name, 'ci:workers'))"
        )
        self.assertIn(
            workers_guard, workflow_job_block(workflow, "restate-postgres-workers")
        )
        self.assertIn(
            workers_guard,
            workflow_job_block(workflow, "restate-postgres-workers-summary"),
        )

        # The matrix now comes from `scripts/ci_plan.py postgres-matrix`, so the
        # bracket is asserted where it is decided. PG16 is the sole primary lane
        # and runs on every event; the PG14/PG18 compatibility lanes only compare
        # the live catalog artifact, so they never run on a pull request and
        # run on a schema merge group and on workflow_dispatch — no schema
        # change reaches trunk without all three majors. The focused contract
        # tests in test_ci_plan.py evaluate per-role step selection.
        postgres = workflow_job_block(workflow, "postgres-store")
        self.assertIn("POSTGRES_PRIMARY: ${{ needs.plan.outputs.postgres_primary }}", postgres)
        self.assertIn(
            "POSTGRES_COMPATIBILITY: ${{ needs.plan.outputs.postgres_compatibility }}",
            postgres,
        )
        for event, expected in (
            ("pull_request", [("16", "primary")]),
            ("merge_group", [("16", "primary")]),
            (
                "workflow_dispatch",
                [("14", "compatibility"), ("16", "primary"), ("18", "compatibility")],
            ),
        ):
            with self.subTest(event=event):
                self.assertEqual(
                    expected,
                    [
                        (leg["postgres"], leg["role"])
                        for leg in plan["postgres_matrix"](event)
                    ],
                )
        for event, expected in (
            ("pull_request", [("16", "primary")]),
            (
                "merge_group",
                [("14", "compatibility"), ("16", "primary"), ("18", "compatibility")],
            ),
        ):
            with self.subTest(event=event, schema=True):
                self.assertEqual(
                    expected,
                    [
                        (leg["postgres"], leg["role"])
                        for leg in plan["postgres_matrix"](event, True)
                    ],
                )

        # postgres-store is gated on the path-derived stores family, so on a
        # docs-only diff a skipped matrix job is accepted.
        pr_needs = {
            job: {**value, "outputs": dict(value.get("outputs", {}))}
            for job, value in needs.items()
        }
        pr_needs["plan"]["outputs"] = dict.fromkeys(plan["FAMILIES"], "false") | {
            "docs_only": "true",
            "fail_open": "false",
        }
        for job in dispatch_only:
            pr_needs[job] = {"result": "skipped", "outputs": {}}
        for job in ("worker-artifacts", "restate-postgres-workers", "restate-postgres-workers-summary"):
            pr_needs[job] = {"result": "success", "outputs": {}}
        for job in plan["GATED_JOBS"]:
            pr_needs[job] = {"result": "skipped", "outputs": {}}
        for job in plan["BAZEL_TEST_JOBS"]:
            pr_needs[job] = {"result": "skipped", "outputs": {}}
        self.assertEqual([], evaluate(pr_needs, "pull_request"))

        # The release gate is what makes the full profile mandatory: a release
        # SHA is certified only by a green workflow_dispatch CI run.
        release = (ROOT / ".github" / "workflows" / "release.yml").read_text(
            encoding="utf-8"
        )
        self.assertIn('run.get("event") == "workflow_dispatch"', release)
        self.assertNotIn('("push", "workflow_dispatch")', release)
        self.assertIn("no full-profile (workflow_dispatch) CI ", release)

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
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")

        self.assertNotIn("confidence-fast:", workflow)
        self.assertNotIn("confidence-fast-summary:", workflow)
        self.assertIn("confidence:", confidence_workflow)
        self.assertNotIn("Confidence gate fast lane", workflow)
        self.assertNotIn("bash scripts/confidence-gate.sh fast\n", workflow)
        for shard in FAST_SHARDS:
            self.assertIn(shard, gate)

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
            str(pathlib.PurePosixPath(computed_relative["full"]).parent / "**"),
            str(
                pathlib.PurePosixPath(computed_relative["sim-search:2/9"]).parent
                / "**"
            ),
        }
        expected_consumed_paths.update(
            f"target/confidence/stages/{stage}/**"
            for stage in ("harnesses", "generated-${{ matrix.shard }}", "minimizer", "backends",
                           "coverage", "mutation-core", "mutation-sim", "mutation-authority",
                           "mutation-packages-${{ matrix.package }}-${{ matrix.shard }}")
        )
        self.assertCountEqual(consumed_paths, expected_consumed_paths)

        self.assertNotIn("path: target/confidence/fast/${{ matrix.shard }}", workflow)
        self.assertNotIn("  confidence-fast:", workflow)
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
        for leaf in (
            "store_contract_state_machine",
            "runtime_persistence_state_machine",
            "session_graph_state_machine",
        ):
            self.assertIn(
                f"--test conformance_memory \\\n      {leaf}", scenario_harnesses
            )
            for package, target in (
                ("lash-sqlite-store", "conformance_memory__test"),
                ("lash-sqlite-store", "conformance__test"),
                ("lash-postgres-store", "conformance__test"),
            ):
                self.assertIn(
                    f"kiln run //crates/{package}:{target} -- {leaf}",
                    justfile,
                )
            self.assertNotIn(f"conformance::tests::{leaf}", scenario_harnesses)
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

    def test_every_script_self_test_is_run_by_ci(self) -> None:
        # The self-test list is enumerated by hand, so a new gate's own test can
        # be written, pass locally, and never run again — which is exactly what
        # happened to the judged-runbook matrix test: the parity claim rested on
        # a gate CI did not execute. Discovering the files makes forgetting one
        # a red test rather than a silent hole.
        workflow = WORKFLOW.read_text(encoding="utf-8")
        discovered = sorted(
            path.name for path in (ROOT / "scripts").glob("test_*.py")
        )
        self.assertGreater(len(discovered), 5, "the self-test discovery found nothing")
        missing = [
            name for name in discovered if f"python3 scripts/{name}" not in workflow
        ]
        self.assertEqual(
            missing,
            [],
            f"these script self-tests exist but CI never runs them: {missing}",
        )

    def test_push_gate_runs_the_gates_whose_self_tests_it_runs(self) -> None:
        push_gate = PUSH_GATE.read_text(encoding="utf-8")
        pre_commit = PRE_COMMIT_CONFIG.read_text(encoding="utf-8")
        self_tests = shell_function_body(push_gate, "run_release_script_tests")

        # A self-test proves the gate works; only the gate proves the tree does.
        # This script ran `test_check_service_gate_pinning.py` and
        # `test_check_transcript_diff.py` while running neither gate, so both
        # CI-blocking failures were invisible until CI.
        self.assertIn("python3 scripts/check_service_gate_pinning.py", push_gate)
        self.assertIn("python3 scripts/check-transcript-diff.py", push_gate)

        # Locally a gate may live in this script or in the prek hooks; what it
        # may not do is exist only as a self-test. Comment lines are stripped
        # first: the omissions block below names the CI-only gates, and a gate
        # explained there is precisely one that does not run.
        local = "\n".join(
            line
            for line in f"{push_gate}\n{pre_commit}".splitlines()
            if not line.lstrip().startswith("#")
        )
        for name in re.findall(r"scripts/(test_check_[a-z0-9_]+)\.py", self_tests):
            stem = name[len("test_") :]
            candidates = [
                ROOT / "scripts" / f"{spelling}{suffix}"
                for spelling in (stem, stem.replace("_", "-"))
                for suffix in (".py", ".sh")
            ]
            gates = [path for path in candidates if path.exists()]
            with self.subTest(self_test=name):
                self.assertTrue(gates, f"{name} tests no discoverable gate")
                self.assertTrue(
                    any(f"scripts/{path.name}" in local for path in gates),
                    f"{name} runs here but its gate runs only in CI",
                )

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

    def test_asserting_operator_e2es_are_in_functional_matrix(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        functional = (workflow_job_block(workflow, "functional-e2e")
                      + workflow_job_block(workflow, "functional-e2e-process-operations"))

        for name, recipe in (
            ("process-operations", "process-operations-e2e"),
            ("version-bump-recreation", "version-bump-recreation-e2e"),
            ("session-lease-triage", "session-lease-triage-e2e"),
        ):
            self.assertIn(f"- name: {name}", functional)
            self.assertIn(f"recipe: {recipe}", functional)
            self.assertIn(f"artifact: {name}", functional)

        for artifact_dir in (
            "process-operations",
            "version-bump-recreation",
            "session-lease-triage",
        ):
            self.assertIn(
                f"target/functional-e2e-artifacts/{artifact_dir}", functional
            )
        self.assertIn("if: failure() && matrix.artifact != 'none'", functional)
        self.assertIn("Upload functional E2E failure artifacts", functional)

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
            # Re-pinned from 5000 (FIG-3222): every shard of run 35091816279 was
            # cancelled at the 100-minute job cap without writing a search
            # summary, so 5000 was a number the lane never reached. The gate
            # carries the sizing arithmetic.
            'search_seeds="${LASH_SIM_FULL_SEEDS:-$SIM_SEARCH_FULL_SEEDS}"',
            'search_max_boundaries="${LASH_SIM_FULL_MAX_BOUNDARIES:-2000}"',
            'local search_shard="${LASH_SIM_SHARD:-1/1}"',
            "--mode search",
            '--shard "$search_shard"',
            # Shards are bounded by wall clock, not just the seed estimate: a
            # shard that runs out of time still writes a summary.
            "sim_search_pass_budget_seconds",
            "--time-budget",
            '"reached_seeds": counts.get("reached_seeds")',
            'counts.get("reached_seeds") or 0) < min_seeds',
            "sim search lane must run in search mode",
        ]
        for snippet in required_gate_snippets:
            self.assertIn(snippet, gate)

        # The fast lane is the merge gate: its generated sim lane keeps the
        # binary's fast-random defaults and never runs the search lane.
        self.assertIn('if [ "$lane" = "fast" ]; then\n    return\n  fi', gate)
        self.assertNotIn("scheduled-depth", gate)
        self.assertNotIn("BROAD_SCHEDULED_DEPTH", gate)

        # Weekly full confidence partitions the complete search seed space in
        # one matrix, including shard 1; generated simulation is separate.
        required_confidence_snippets = [
            "sim-search:",
            'bash scripts/confidence-gate.sh "sim-search:${{ matrix.shard }}/9"',
            "shard: [1, 2, 3, 4, 5, 6, 7, 8, 9]",
            "LASH_SIM_SHARD",
            "${{ matrix.shard }}/9",
        ]
        for snippet in required_confidence_snippets:
            self.assertIn(snippet, confidence_workflow)

        # The per-merge CI workflow must not run search
        # shards or override sim budgets.
        self.assertNotIn("sim-search", workflow)
        self.assertNotIn("LASH_SIM_SHARD", workflow)
        self.assertNotIn("LASH_SIM_FULL_SEEDS", workflow)

    def test_sim_search_and_mutation_sim_fit_their_job_caps(self) -> None:
        """Both lanes were cancelled at 100 minutes in run 35091816279.

        A job cancelled at its cap produces no evidence at all, so the seed
        budget and the mutant budget each have to be stated against the cap
        with the shared-build download counted as the fixed cost it is.
        """
        gate = GATE.read_text(encoding="utf-8")
        confidence_workflow = CONFIDENCE_WORKFLOW.read_text(encoding="utf-8")

        # The full lane is sized, not left at a number no shard has reached,
        # and the wall-clock bound is derived from the job cap minus the
        # measured fixed cost rather than the seed estimate alone.
        full_seeds = shell_int_constant(gate, "SIM_SEARCH_FULL_SEEDS")
        min_seeds = shell_int_constant(gate, "SIM_SEARCH_MIN_SEEDS")
        job_cap_seconds = shell_int_constant(gate, "SIM_SEARCH_JOB_CAP_SECONDS")
        setup_seconds = shell_int_constant(gate, "SIM_SEARCH_SETUP_SECONDS")
        shards = 9
        per_shard = full_seeds // shards
        sim_search_cap = int(
            re.search(
                r"^    timeout-minutes: (\d+)$",
                workflow_job_block(confidence_workflow, "sim-search"),
                re.MULTILINE,
            ).group(1)
        )
        # The gate's job cap is the workflow's timeout-minutes on the
        # sim-search job, and the setup constant is the measured 25 minutes of
        # shared-build download/restore plus checkout before the script runs.
        self.assertEqual(job_cap_seconds, sim_search_cap * 60)
        self.assertEqual(setup_seconds, 25 * 60)
        lane_budget_seconds = job_cap_seconds - setup_seconds
        # Each pass is handed the remaining lane budget divided by the passes
        # still to run, so the corpus pass inherits the search pass's slack.
        self.assertIn(
            "remaining=$((job_cap_seconds - setup_seconds - (SECONDS - script_started_at)))",
            gate,
        )
        self.assertIn('printf \'%s\\n\' "$((remaining / passes_left))"', gate)
        self.assertIn(
            'search_budget_args=(--time-budget "$(sim_search_pass_budget_seconds 2)")',
            gate,
        )
        self.assertIn(
            'corpus_budget_args=(--time-budget "$(sim_search_pass_budget_seconds 1)")',
            gate,
        )
        # The estimate still has to be plausible: a shard runs two search
        # passes (the search lane and the named regression corpus). Measured
        # end to end through the gate at 2000 max boundaries, the pair costs
        # about 105 s of setup plus 112 s per seed.
        self.assertLessEqual(
            105 + per_shard * 112,
            lane_budget_seconds,
            f"{per_shard} seeds per shard do not fit the {sim_search_cap}-minute cap",
        )
        self.assertEqual(full_seeds % shards, 0, "seeds must divide over the shards")
        self.assertGreater(per_shard, min_seeds)

        # The shard records what it actually cost, so the estimate above is
        # re-pinned from a measurement rather than re-guessed.
        self.assertIn('local search_seconds=$((SECONDS - search_started_at))', gate)
        self.assertIn('"search_seconds": int(search_seconds),', gate)
        self.assertIn('artifact["corpus_seconds"] = int(corpus_seconds)', gate)
        self.assertIn('artifact["shard_seconds"]', gate)

        # mutation-sim is a mutant count, not a seed budget: 49 mutants at the
        # 2.03 min/mutant the cancelled run measured, plus the same 23 minutes
        # of fixed cost, with margin for the 180 s per-test cap.
        mutation_sim_cap = int(
            re.search(
                r"^    timeout-minutes: (\d+)$",
                workflow_job_block(confidence_workflow, "confidence-mutation-sim"),
                re.MULTILINE,
            ).group(1)
        )
        self.assertGreaterEqual(mutation_sim_cap, 23 + 49 * 2.03)

    def test_mutation_packages_legs_fit_their_job_cap(self) -> None:
        """Run 35117123483 cancelled three package legs at the 100-minute cap.

        A cancelled leg writes no verdict at all, and the mutant spaces are
        far wider than one job can sweep (protocol-rlm alone listed 1,641
        mutants), so the matrix fans each package out into legs that each
        judge a bounded slice. The slice index rotates with the run number so
        successive runs sweep the space instead of re-judging one prefix.
        """
        gate = GATE.read_text(encoding="utf-8")
        stage = (ROOT / "scripts/ci/confidence-stage.sh").read_text(encoding="utf-8")
        confidence = yaml.safe_load(CONFIDENCE_WORKFLOW.read_text())
        job = confidence["jobs"]["confidence-mutation-packages"]

        # Every leg is bounded: the stage opts in, the matrix hands each leg a
        # shard coordinate, and the run number rotates the judged slice.
        self.assertIn("LASH_MUTATION_PACKAGES_BOUNDED=1", stage)
        run_step = next(
            s for s in job["steps"] if s.get("name") == "Run mutation-packages"
        )
        self.assertEqual(
            "${{ matrix.shard }}/${{ matrix.shards }}",
            run_step["env"]["LASH_MUTATION_PACKAGES_SHARD"],
        )
        self.assertEqual(
            "${{ github.run_number }}", run_step["env"]["LASH_MUTATION_RUN_INDEX"]
        )

        # The matrix fans each package into a contiguous 1..legs set of legs.
        expected_legs = {
            "lash-internal-core": 2,
            "lash-internal-lashlang": 4,
            "lash-internal-protocol-rlm": 4,
            "lash-internal-protocol-standard": 1,
            "lash-internal-sqlite-store": 3,
            "lash-internal-postgres-store": 4,
        }
        rows = job["strategy"]["matrix"]["include"]
        self.assertEqual(sorted(expected_legs), sorted({r["package"] for r in rows}))
        for package, leg_count in expected_legs.items():
            legs = [r for r in rows if r["package"] == package]
            self.assertEqual(
                list(range(1, leg_count + 1)),
                sorted(r["shard"] for r in legs),
                f"{package} legs are not a contiguous 1..{leg_count} set",
            )
            self.assertTrue(
                all(r["shards"] == leg_count for r in legs),
                f"{package} legs disagree on the leg count",
            )
        self.assertIs(False, job["strategy"]["fail-fast"])

        # Legs are distinguishable in job names, out dirs and artifact names,
        # so one leg's evidence can never overwrite or impersonate another's.
        upload = next(s for s in job["steps"] if "upload-artifact@" in s.get("uses", ""))
        self.assertIn("${{ matrix.package }}-${{ matrix.shard }}", upload["with"]["name"])
        self.assertIn("${{ matrix.package }}-${{ matrix.shard }}", upload["with"]["path"])
        self.assertIn(
            "${{ matrix.package }}-${{ matrix.shard }}",
            run_step["env"]["LASH_CONFIDENCE_OUT_DIR"],
        )
        self.assertIn("${{ matrix.package }}-${{ matrix.shard }}", job["name"])

        # The gate counts the space with `cargo mutants --list`, derives the
        # slice from the leg coordinate plus the run index, and hands it to
        # cargo-mutants as --shard in both passes.
        shard_fn = shell_function_body(gate, "mutation_packages_shard")
        self.assertIn("--list", shard_fn)
        self.assertIn("LASH_MUTATION_PACKAGES_SHARD", shard_fn)
        self.assertIn("LASH_MUTATION_RUN_INDEX", shard_fn)
        for function in ("run_mutation_smoke", "run_mutation_full"):
            body = shell_function_body(gate, function)
            self.assertIn('mutation_packages_shard "$package"', body)
            self.assertIn("--shard", body)
            self.assertIn('${LASH_MUTATION_PACKAGES_BOUNDED:-0}', body)
        self.assertIn(
            "LASH_MUTATION_SMOKE_SHARD", shell_function_body(gate, "run_mutation_smoke")
        )
        self.assertIn(
            "LASH_MUTATION_FULL_SHARD", shell_function_body(gate, "run_mutation_full")
        )
        self.assertIn("LASH_MUTATION_PACKAGES_SHARD must be", gate)
        self.assertIn(
            "mutation-shard.json", shell_function_body(gate, "run_mutants_recorded")
        )

        # Budget arithmetic: fixed cost + smoke slice + full slice must fit
        # the job cap read out of the workflow, at the per-mutant wall clock
        # run 35117123483 measured at --jobs 2 (smoke at the 180 s cap, full
        # at the 600 s cap; each slice also pays the unmutated baseline).
        cap = job["timeout-minutes"]
        smoke_budget = shell_int_constant(gate, "MUTATION_PACKAGES_SMOKE_MUTANTS")
        full_budgets = shell_assoc_array(gate, "MUTATION_PACKAGES_FULL_MUTANTS")
        shell_int_constant(gate, "MUTATION_PACKAGES_FULL_MUTANTS_DEFAULT")
        self.assertEqual(sorted(expected_legs), sorted(full_budgets))
        smoke_minutes_per_mutant = {
            "lash-internal-core": 2.5,
            "lash-internal-lashlang": 0.8,
            "lash-internal-protocol-rlm": 1.3,
            "lash-internal-protocol-standard": 0.2,
            "lash-internal-sqlite-store": 1.5,
            "lash-internal-postgres-store": 1.0,
        }
        full_minutes_per_mutant = {
            "lash-internal-core": 4.0,
            "lash-internal-lashlang": 0.8,
            "lash-internal-protocol-rlm": 1.3,
            "lash-internal-protocol-standard": 0.2,
            "lash-internal-sqlite-store": 1.5,
            "lash-internal-postgres-store": 4.0,
        }
        for package in expected_legs:
            with self.subTest(package=package):
                leg_minutes = (
                    25
                    + smoke_budget * smoke_minutes_per_mutant[package]
                    + 8
                    + int(full_budgets[package]) * full_minutes_per_mutant[package]
                    + 12
                )
                self.assertLessEqual(
                    leg_minutes,
                    cap,
                    f"{package}: {leg_minutes:.0f}-minute leg does not fit "
                    f"the {cap}-minute cap",
                )

    def test_mutation_packages_bounded_leg_rotates_slices(self) -> None:
        """The leg coordinate plus the run index must pick distinct slices."""
        gate = GATE.read_text(encoding="utf-8")
        shard_fn = shell_function_definition(gate, "mutation_packages_shard")
        smoke_fn = shell_function_definition(gate, "run_mutation_smoke")
        full_fn = shell_function_definition(gate, "run_mutation_full")
        harness = f"""\
set -euo pipefail
{shard_fn}
{smoke_fn}
{full_fn}
area_mutation_file_args=()
MUTATION_PACKAGES_SMOKE_MUTANTS=12
MUTATION_EXCLUDED_TEST_NAME='durable_fault_matrix_real_cargo_filters_chunk_'
declare -A MUTATION_PACKAGES_FULL_MUTANTS=([pkg-x]="5")
MUTATION_PACKAGES_FULL_MUTANTS_DEFAULT=4
selected_packages=(pkg-x)
out_dir="$1"
mutation_jobs=2
step() {{ :; }}
require_tool() {{ :; }}
cargo() {{
  if [[ "$*" == *--list* ]]; then seq 1 23; return 0; fi
}}
run_mutants_recorded() {{ printf 'RECORDED %s\\n' "$*"; }}
run_postgres_mutants_recorded() {{ printf 'PG %s\\n' "$*"; }}
"""
        # 23 mutants at budgets 12 (smoke, denom 2) and 5 (full, denom 5).
        # Two legs plus the run index walk consecutive slices of each space.
        cases = [
            # (run, leg spec) -> (smoke shard, full shard)
            (1, "1/2", "1/2", "1/5"),
            (1, "2/2", "2/2", "2/5"),
            (2, "1/2", "1/2", "3/5"),
            (2, "2/2", "2/2", "4/5"),
            (3, "1/2", "1/2", "5/5"),
        ]
        for run_index, leg, smoke_shard, full_shard in cases:
            with self.subTest(run=run_index, leg=leg):
                env = dict(
                    os.environ,
                    LASH_MUTATION_PACKAGES_BOUNDED="1",
                    LASH_MUTATION_PACKAGES_SHARD=leg,
                    LASH_MUTATION_RUN_INDEX=str(run_index),
                )
                result = subprocess.run(
                    ["bash", "-c", harness + "\nrun_mutation_smoke\nrun_mutation_full", "t", "/tmp/x"],
                    env=env,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(0, result.returncode, result.stderr)
                self.assertIn(f"--shard {smoke_shard} ", result.stdout)
                self.assertIn(f"--shard {full_shard} ", result.stdout)

        # Without the bound flag the full pass stays an unsharded sweep and
        # the smoke canary keeps its historical 1/64 slice; explicit shard
        # selectors still win for local reproduction.
        env = dict(os.environ)
        for name in (
            "LASH_MUTATION_PACKAGES_BOUNDED",
            "LASH_MUTATION_PACKAGES_SHARD",
            "LASH_MUTATION_RUN_INDEX",
            "LASH_MUTATION_SMOKE_SHARD",
            "LASH_MUTATION_FULL_SHARD",
        ):
            env.pop(name, None)
        result = subprocess.run(
            ["bash", "-c", harness + "\nrun_mutation_smoke\nrun_mutation_full", "t", "/tmp/x"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("--shard 1/64 ", result.stdout)
        full_line = next(
            line for line in result.stdout.splitlines() if "full mutation" in line
        )
        self.assertNotIn("--shard", full_line)

        env["LASH_MUTATION_PACKAGES_BOUNDED"] = "1"
        env["LASH_MUTATION_FULL_SHARD"] = "3/7"
        result = subprocess.run(
            ["bash", "-c", harness + "\nrun_mutation_full", "t", "/tmp/x"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn("--shard 3/7 ", result.stdout)

    def test_mutation_package_loops_skip_the_real_cargo_fault_matrix_probes(self) -> None:
        """The fault-matrix chunk tests fork a real `cargo test` each and alone
        exceed the baseline's per-test timeout; every package-scoped
        cargo-mutants run must forward the same skip the Bazel targets use."""
        gate = GATE.read_text(encoding="utf-8")
        self.assertIn(
            "MUTATION_EXCLUDED_TEST_NAME='durable_fault_matrix_real_cargo_filters_chunk_'",
            gate,
        )
        for function in (
            "run_mutation_smoke",
            "run_mutation_full",
            "run_area_targeted_mutation_evidence",
        ):
            body = shell_function_body(gate, function)
            invocations = body.count("cargo mutants")
            self.assertGreater(invocations, 0, function)
            self.assertEqual(
                invocations,
                body.count('-- -- --skip="${MUTATION_EXCLUDED_TEST_NAME}"'),
                f"{function}: every cargo-mutants call must skip the real-Cargo "
                "fault-matrix probes",
            )

        smoke_fn = shell_function_definition(gate, "run_mutation_smoke")
        harness = f"""\
set -euo pipefail
{smoke_fn}
MUTATION_PACKAGES_SMOKE_MUTANTS=4
MUTATION_EXCLUDED_TEST_NAME='durable_fault_matrix_real_cargo_filters_chunk_'
selected_packages=(pkg-x)
area_mutation_file_args=()
out_dir="$1"
mutation_jobs=2
step() {{ :; }}
require_tool() {{ :; }}
cargo() {{ :; }}
run_mutants_recorded() {{ printf 'RECORDED %s\\n' "$*"; }}
"""
        env = dict(os.environ, LASH_MUTATION_SMOKE_SHARD="1/2")
        result = subprocess.run(
            ["bash", "-c", harness + "\nrun_mutation_smoke", "t", "/tmp/x"],
            env=env,
            capture_output=True,
            text=True,
        )
        self.assertEqual(0, result.returncode, result.stderr)
        self.assertIn(
            "-- -- --skip=durable_fault_matrix_real_cargo_filters_chunk_",
            result.stdout,
        )

    def test_panic_gate_scans_ordinary_logs_but_not_mutants_out(self) -> None:
        """A caught mutant's own log legitimately contains `panicked at`; the
        gate must fail on panic markers only outside `mutants.out/`."""
        gate = GATE.read_text(encoding="utf-8")
        function = shell_function_definition(gate, "assert_no_panics_in_artifacts")
        harness = f"{function}\nout_dir=\"$1\"\nassert_no_panics_in_artifacts"
        with tempfile.TemporaryDirectory() as tmp:
            out_dir = pathlib.Path(tmp) / "out"
            (out_dir / "mutants.out" / "log").mkdir(parents=True)
            (out_dir / "mutants.out" / "log" / "mutant.log").write_text(
                "thread '<unnamed>' panicked at crates/pkg/src/lib.rs:1:1\n"
            )
            clean = subprocess.run(
                ["bash", "-c", harness, "t", str(out_dir)],
                capture_output=True,
                text=True,
            )
            self.assertEqual(0, clean.returncode, clean.stderr)
            self.assertIn("panic gate: clean", clean.stdout)

            (out_dir / "sim-evidence.log").write_text(
                "panicked at crates/lash-sim/src/runner.rs:9:3\n"
            )
            dirty = subprocess.run(
                ["bash", "-c", harness, "t", str(out_dir)],
                capture_output=True,
                text=True,
            )
            self.assertEqual(1, dirty.returncode, dirty.stderr)
            self.assertIn("panic gate: FAILED", dirty.stderr)

    def test_lane_composition_is_declared_once_per_path(self) -> None:
        """Four hand-written copies of the same composition is three too many.

        The fast shard dispatch reads its steps from the same table the
        schedule declares, and the two compositions the non-sharded paths used
        to repeat are functions with more than one caller.
        """
        gate = GATE.read_text(encoding="utf-8")

        steps = shell_assoc_array(gate, "confidence_fast_shard_steps")
        self.assertEqual({*FAST_SHARDS, "summary"}, set(steps))

        # The shards the composition table knows are exactly the shard
        # selectors the schedule table declares, and each shard's suite name is
        # its own -- a shard cannot compose a suite it does not schedule.
        table_body = gate.split("confidence_schedule_table=(\n", 1)[1].split("\n)\n", 1)[0]
        rows = [
            ast.literal_eval(line.strip())
            for line in table_body.splitlines()
            if line.strip().startswith('"')
        ]
        scheduled_suites: dict[str, set[str]] = {}
        for row in rows:
            selector, _, suite = row.split("|", 3)[:3]
            if selector.startswith("fast:") and selector != "fast:all":
                scheduled_suites.setdefault(selector.removeprefix("fast:"), set()).add(suite)
        self.assertEqual(set(steps), set(scheduled_suites))
        for shard, suites in scheduled_suites.items():
            self.assertEqual({shard}, suites, shard)

        # Every step is a function this script defines, so a typo is a missing
        # function at parse time rather than a silently skipped suite.
        defined = set(re.findall(r"^([a-zA-Z_][a-zA-Z0-9_]*)\(\) \{$", gate, re.MULTILINE))
        for shard, step_list in steps.items():
            self.assertTrue(step_list.strip(), shard)
            for step in step_list.split():
                self.assertIn(step, defined, (shard, step))

        # run_fast_shard composes; it does not carry its own arm per shard.
        dispatch = shell_function_definition(gate, "run_fast_shard")
        self.assertIn('steps="${confidence_fast_shard_steps[$fast_shard]:-}"', dispatch)
        self.assertIn("unknown fast shard", dispatch)
        for shard in FAST_SHARDS:
            self.assertNotIn(shard, dispatch, shard)

        # The two compositions the fast area-scoped path and the non-fast lane
        # used to spell out are shared, not copied. `run_core_suites` is called
        # by both of those paths; `write_sim_lane_evidence` by the non-fast
        # lane and by the sim-generated shard through the table above.
        self.assertEqual(
            2, len(re.findall(r"^\s*run_core_suites$", gate, re.MULTILINE))
        )
        self.assertEqual(
            1, len(re.findall(r"^\s*write_sim_lane_evidence$", gate, re.MULTILINE))
        )
        self.assertIn("write_sim_lane_evidence", steps["sim-generated"])

        # Neither composition may grow a second copy of the other's body.
        core = shell_function_definition(gate, "run_core_suites")
        evidence = shell_function_definition(gate, "write_sim_lane_evidence")
        for writer in (
            "write_sim_lane_declarations",
            "write_full_lane_prerequisites",
            "write_postgres_effect_history_status",
            "write_restate_postgres_workers_e2e_lane_status",
        ):
            self.assertIn(writer, evidence, writer)
            self.assertEqual(
                1, len(re.findall(rf"^\s*{writer}$", gate, re.MULTILINE)), writer
            )
        for suite in ("run_scenario_harnesses", "run_state_machine_and_fault_matrix"):
            self.assertIn(suite, core, suite)

    def test_release_is_manual_and_requires_a_green_main_commit(self) -> None:
        workflow = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        required_snippets = [
            "workflow_dispatch:",
            "release_sha:",
            'requested="${REQUESTED_SHA:-origin/main}"',
            'git merge-base --is-ancestor "${sha}" origin/main',
            'gh run list --workflow ci.yml --commit "${sha}"',
            'run.get("event") == "workflow_dispatch"',
            "run = matching[0]",
            'run.get("conclusion") != "success"',
            "release refused: target ",
            "run.get('databaseId')",
            "run.get('url', 'URL unavailable')",
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
        self.assertIn("needs: [prepare-release, validate-release-ref, package-crates]", publish_crates)
        self.assertIn("runs-on: ubuntu-24.04", validate_release)
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
        release_cache_workflow = RELEASE_CACHE_WORKFLOW.read_text(encoding="utf-8")
        perf = PERF_WORKFLOW.read_text(encoding="utf-8")
        release = RELEASE_WORKFLOW.read_text(encoding="utf-8")
        release_cache = workflow_job_block(release_cache_workflow, "linux-release-cache")

        self.assertIn("workflow_dispatch:", perf)
        # The full profile is dispatched manually before release work; it
        # never runs on a schedule, push or pull_request.
        self.assertNotIn("schedule:", perf)
        self.assertNotIn("pull_request:", perf)
        self.assertNotIn("push:", perf)
        self.assertIn("runs-on: ubuntu-24.04", perf)
        self.assertIn(
            "uses: Swatinem/rust-cache@"
            "6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2",
            release_cache,
        )
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

        self.assertIn("image: postgres:16-alpine", perf)
        self.assertRegex(
            release,
            r"image: postgres@sha256:[0-9a-f]{64} # postgres:16-alpine",
        )
        for workflow_with_postgres in (perf, release):
            self.assertIn("LASH_POSTGRES_DATABASE_URL:", workflow_with_postgres)

        # These two legs run no `cargo test`, so they carry no
        # `LASH_REQUIRE_POSTGRES`: their Postgres service exists for one
        # consumer, the store-hardening perf scenario, which refuses to run
        # without the URL instead of degrading to a SQLite-only measurement.
        # Pin both halves — a deleted scenario would leave the service
        # decorative, and a softened refusal would turn a missing database into
        # a green perf report with silently fewer phases. The refusal lives in
        # the scenario dispatch (phase_probe.rs), which errors unconditionally
        # for store-hardening when no URL is configured.
        perf_scenarios = PERF_SCENARIOS_RS.read_text(encoding="utf-8")
        perf_phase_probe = PERF_PHASE_PROBE_RS.read_text(encoding="utf-8")
        self.assertIn('"store_hardening_hot_paths"', perf_scenarios)
        self.assertIn(
            "requires LASH_POSTGRES_DATABASE_URL or DATABASE_URL "
            "(the full perf workflows provide it)",
            perf_phase_probe,
        )
        self.assertIn(
            "RuntimePerfScenario::StoreHardeningHotPaths => {\n"
            "            let postgres_database_url = "
            "configured_postgres_database_url().ok_or_else(|| {",
            perf_phase_probe,
        )

    def test_secret_bearing_workflows_pin_external_actions_by_sha(self) -> None:
        for path in (RELEASE_WORKFLOW,):
            workflow = path.read_text(encoding="utf-8")
            actions = re.findall(
                r"^\s+uses: ([A-Za-z0-9_.-]+/[A-Za-z0-9_.-]+)@([^\s#]+)"
                r"(?:\s+#\s*(\S.*))?$",
                workflow,
                flags=re.MULTILINE,
            )
            self.assertGreater(len(actions), 0, path.name)
            for action, ref, version_comment in actions:
                with self.subTest(workflow=path.name, action=action):
                    self.assertRegex(ref, r"^[0-9a-f]{40}$")
                    self.assertRegex(
                        version_comment or "",
                        r"^(?:v[0-9]+(?:\.[0-9]+){0,2}|stable)$",
                    )

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
            '"required_restate_postgres_workers_e2e": "$(scheduled_artifact_path restate_postgres_workers_e2e',
            '"restate_postgres_workers_e2e_status": "$(restate_postgres_workers_e2e_status)"',
            "run_restate_postgres_workers_e2e",
            '"status": "not_run"',
            '"reason": "distributed Restate/Postgres/S3 worker e2e is full-lane-only"',
        ]

        for snippet in required_snippets:
            self.assertIn(snippet, gate)

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
        main = gate[gate.rindex("\nrun_core_suites\n") :]

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
        self.assertIn("lash-internal-sqlite-store", critical_packages)
        self.assertIn("lash-internal-postgres-store", critical_packages)
        for function_name in ("run_mutation_smoke", "run_mutation_full"):
            body = shell_function_body(gate, function_name)
            loop_headers = re.findall(
                r"^\s*for\s+package\s+in\s+(.+);\s*do\s*$", body, re.MULTILINE
            )
            self.assertEqual(['"${selected_packages[@]}"'], loop_headers)
            self.assertIn(
                'if [ "$package" = "lash-internal-postgres-store" ]; then', body
            )
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

    def test_every_conformance_run_builds_its_spawn_helpers_first(self) -> None:
        """The cold-process suites spawn example binaries nothing else builds.

        `lash_conformance::helper_executable` resolves the helper from the
        *running test binary's own* `<target>/<profile>/examples`, and both
        helpers carry `required-features = ["testing"]`, so no
        `cargo test --test conformance` invocation produces them. One shared
        function builds them; this pins that every conformance command in the
        gate is reached through it, into the target directory it runs against.
        """
        gate = GATE.read_text(encoding="utf-8")

        definition = shell_function_definition(gate, "build_conformance_helpers")
        for helper in (
            "-p lash-internal-sqlite-store --locked --features testing",
            "--example sqlite-await-event-helper",
            "-p lash-internal-postgres-store --locked --features testing",
            "--example postgres-await-event-helper",
        ):
            self.assertIn(helper, definition)

        # A conformance command is only covered by a helper build that runs inside the same
        # function.
        built = False
        conformance_commands = 0
        for line in gate.splitlines():
            stripped = line.strip()
            if stripped.startswith("#"):
                continue
            # Only a top-level definition opens a new scope; the nested
            # `cleanup_*` traps live inside the function they belong to.
            if re.match(r"^[a-zA-Z_][a-zA-Z0-9_]*\(\) \{$", line):
                built = False
                continue
            if stripped.split()[0:1] == ["build_conformance_helpers"]:
                built = True
                continue
            if "--test conformance" in stripped:
                conformance_commands += 1
                self.assertTrue(
                    built,
                    f"conformance run not preceded by build_conformance_helpers: {stripped}",
                )
        self.assertGreaterEqual(conformance_commands, 10)

        # The coverage stage is the one runner with its own target directory:
        # cargo-llvm-cov compiles into `<target>/llvm-cov-target`, so a helper
        # in `<target>/debug/examples` is invisible to the binaries it runs and
        # the four cold-process tests fail on spawn with ENOENT. Its build must
        # name that directory, and must ask cargo-llvm-cov where it is rather
        # than spelling the layout a second time.
        self.assertIn('target_args=(--target-dir "$1")', definition)
        coverage = shell_function_body(gate, "run_coverage_blind_spots")
        self.assertIn("cargo llvm-cov show-env --export-prefix", coverage)
        self.assertIn("CARGO_LLVM_COV_TARGET_DIR", coverage)
        self.assertIn("/llvm-cov-target", coverage)
        build_at = coverage.index("build_conformance_helpers")
        # The instrumentation environment is evaluated in a subshell, so it
        # reaches the helper build and nothing else.
        self.assertLess(coverage.index('eval "$llvm_cov_env"'), build_at)
        # And the build runs before the coverage test run it is for.
        test_run_at = next(
            index
            for index, line in enumerate(coverage.splitlines())
            if line.strip() == "--tests \\"
        )
        build_line = next(
            index
            for index, line in enumerate(coverage.splitlines())
            if line.strip().startswith("build_conformance_helpers")
        )
        self.assertLess(build_line, test_run_at)

    def test_every_gate_postgres_container_preloads_pg_stat_statements(self) -> None:
        """The statement-count tests measure through the extension.

        `pg_stat_statements` only exists when it is preloaded at server start,
        so the flag belongs to the container. One shared starter means a later
        container cannot be added without it.
        """
        gate = GATE.read_text(encoding="utf-8")

        definition = shell_function_definition(gate, "start_gate_postgres")
        self.assertIn("-c shared_preload_libraries=pg_stat_statements", definition)
        self.assertIn('bash scripts/docker-pull-with-retry.sh "$gate_postgres_image"', definition)
        self.assertIn('gate_postgres_image="postgres:16-alpine"', gate)

        # `start_gate_postgres` is the only thing in the gate that starts one.
        docker_runs = [
            command
            for command in shell_logical_commands(gate)
            if command.startswith("docker run")
        ]
        self.assertEqual(len(docker_runs), 1)
        self.assertIn('"$gate_postgres_image"', docker_runs[0])

        starts = [
            line.strip()
            for line in gate.splitlines()
            if line.strip().startswith("start_gate_postgres ")
        ]
        self.assertEqual(len(starts), 4, starts)

        # The wrapper CI uses starts the same image the same way.
        wrapper = (ROOT / "scripts" / "ci" / "with-service.sh").read_text(encoding="utf-8")
        self.assertIn("shared_preload_libraries=pg_stat_statements", wrapper)

    def test_postgres_ci_lane_requires_database_configuration(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        gate = GATE.read_text(encoding="utf-8")
        push_gate = PUSH_GATE.read_text(encoding="utf-8")
        postgres_store_job = workflow_job_block(workflow, "postgres-store")

        self.assertIn('LASH_REQUIRE_POSTGRES: "1"', workflow)
        self.assertIn('LASH_CROSS_BACKEND_CASES: "4"', postgres_store_job)

        # Every live-Postgres suite needs both settings, on both dispatch paths.
        # Without them the suites short-circuit on the absent URL, print a skip
        # reason and report `ok` — the cross-backend differential reports
        # "ok in 0.00s" with `compared_backends=[]`, so losing the URL from one
        # suite silently returns the differential to comparing nothing.
        #
        # The require flag is supplied once, at job level, so no step can drop
        # it. The connection URL cannot live there: `scripts/ci/with-service.sh`
        # publishes the database on a free ephemeral port per step, so a
        # job-level literal would name a port nothing listens on. Instead the
        # wrapper that chooses the port exports the URL, and every suite in this
        # job runs inside it — `scripts/test_with_service.py` refuses a store
        # suite that is not wrapped, which is the same "no step can lose it"
        # property enforced at its source rather than by inheritance. Either
        # way a Bazel test spawn inherits nothing from the client environment,
        # so the shared `bazel_test` helper still forwards both by name — and
        # that forwarding is what keeps the PG major an execution-only input,
        # outside every compile action key.
        postgres_job_env = yaml.safe_load(workflow)["jobs"]["postgres-store"]["env"]
        self.assertNotIn("LASH_POSTGRES_DATABASE_URL", postgres_job_env)
        self.assertEqual("1", str(postgres_job_env["LASH_REQUIRE_POSTGRES"]))
        wrapper = (ROOT / "scripts" / "ci" / "with-service.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn("LASH_POSTGRES_DATABASE_URL=postgres://", wrapper)
        for step_name in (
            "Test PostgreSQL catalog compatibility",
            "Test Postgres store (conformance and attempt atomicity)",
            "Test runtime pool-wait binding",
            "Test simulator backend faults on Postgres",
            "Test cross-backend store differential",
        ):
            with self.subTest(step=step_name):
                step = workflow_step_block(postgres_store_job, step_name)
                major = (
                    "${major}"
                    if step_name == "Test PostgreSQL catalog compatibility"
                    else "${POSTGRES_PRIMARY}"
                )
                self.assertIn(
                    f'bash scripts/ci/with-service.sh "pg{major}" --',
                    step,
                )

        store_tests = STORE_TESTS.read_text(encoding="utf-8")
        bazel_helper = store_tests.split("bazel_test() {", 1)[1].split("\n}", 1)[0]
        self.assertIn("--test_env=LASH_POSTGRES_DATABASE_URL", bazel_helper)
        self.assertIn("--test_env=LASH_REQUIRE_POSTGRES", bazel_helper)

        for step_name in (
            "Test PostgreSQL catalog compatibility",
            "Test Postgres store (conformance and attempt atomicity)",
            "Test runtime pool-wait binding",
            "Test cross-backend store differential",
        ):
            with self.subTest(step=step_name):
                step = workflow_step_block(postgres_store_job, step_name)
                bazel, cargo = store_suite_branches(store_suite_for_step(step))
                self.assertTrue(bazel.strip())
                self.assertIn("cargo ", cargo)

        store_suites = workflow_step_block(
            postgres_store_job, "Test Postgres store (conformance and attempt atomicity)"
        )
        # The whole store package runs on every event (FIG-3572): FIG-3595
        # and FIG-3550 broke it while it was dispatch-only.
        self.assertNotIn("if:", store_suites.split("run:", 1)[0])
        self.assertIn("store-tests.sh pg-store", store_suites)
        self.assertIn(
            "//crates/lash-postgres-store:integration__test",
            (ROOT / "tools/bazel/postgres_test_labels.txt").read_text(encoding="utf-8"),
        )
        self.assertIn("if: needs.plan.outputs.stores == 'true'", postgres_store_job)

        # The differential's skip reason and its `compared_backends` inventory
        # go to stderr, which libtest swallows for a passing test: uncaptured
        # output is what makes a real run distinguishable from a skipped one.
        differential_bazel, differential_cargo = store_suite_branches(
            store_suite_for_step(
                workflow_step_block(
                    postgres_store_job, "Test cross-backend store differential"
                )
            )
        )
        self.assertIn("--no-capture", differential_cargo)
        # libtest's spelling of the same flag, plus the Bazel-side switch that
        # actually lets the uncaptured stderr reach the log.
        self.assertIn("--test_arg=--nocapture", differential_bazel)
        self.assertIn("--test_output=all", differential_bazel)
        self.assertIn(
            'LASH_CROSS_BACKEND_CASES="${LASH_CROSS_BACKEND_PR_CASES:-4}"',
            push_gate,
        )
        conformance_calls = [
            command
            for command in shell_logical_commands(gate)
            if re.search(
                r"\bcargo test -p lash-internal-postgres-store\b.*(?:^|\s)--test\s+conformance(?:\s|$)",
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

    def test_store_suites_state_one_selection_in_both_dialects(self) -> None:
        """Each suite's test selection is written once, rendered twice.

        Nine `case` arms used to encode the selection twice, with the flag
        translation (`--run-ignored all` is `--include-ignored`, `-j1` is
        `--test-threads=1`) written as a comment. Parity was hand-asserted for
        three arms and "both halves non-empty" for the rest.

        The counts the audit quoted -- nine suites, six uniform -- counted the
        `case`'s own `*)` arm. The tree has seven suites: four uniform, and
        three that keep explicit arms because their shape varies
        (`pg-catalog-compatibility` runs two invocations; `pg-store` and
        `s3-store` take a generated label file rather than one label).
        """
        script = STORE_TESTS.read_text(encoding="utf-8")
        workflow = WORKFLOW.read_text(encoding="utf-8")

        table_body = script.split("declare -A uniform_store_suites=(\n", 1)[1]
        table_body = table_body.split("\n)\n", 1)[0]
        uniform = dict(
            re.findall(r'^\s*\[([^\]]+)\]="([^"]*)"$', table_body, re.MULTILINE)
        )
        shaped = set(re.findall(r"^  ([a-z0-9-]+)\)$", script, re.MULTILINE))
        suites = set(uniform) | shaped
        dispatched = set(
            re.findall(r"bash scripts/ci/store-tests\.sh ([a-z0-9-]+)", workflow)
        )

        self.assertEqual(4, len(uniform), sorted(uniform))
        self.assertEqual(
            {"pg-catalog-compatibility", "pg-store", "s3-store"}, shaped
        )
        self.assertEqual(suites, dispatched)
        self.assertEqual(7, len(suites), sorted(suites))
        # A suite cannot be in both halves, or the table would be shadowed.
        self.assertEqual(set(), set(uniform) & shaped)

        flag_dialects = {
            "include-ignored": ("--test_arg=--include-ignored", ("--run-ignored all", "--include-ignored")),
            "single-threaded": ("--test_arg=--test-threads=1", ("-j1", "--test-threads=1")),
            "nocapture": ("--test_arg=--nocapture", ("--no-capture", "--nocapture")),
        }

        # Parity for every suite, uniform or shaped: both halves render a real
        # command. The stubs make this the command the script would have run.
        for suite in sorted(suites):
            with self.subTest(suite=suite):
                bazel, cargo = store_suite_branches(suite)
                self.assertTrue(bazel.startswith("bazel test "), bazel)
                self.assertIn("cargo ", cargo)

        for suite, row in sorted(uniform.items()):
            with self.subTest(suite=suite):
                label, test_filter, package, target, runner, flags = row.split("|")
                bazel, cargo = store_suite_branches(suite)
                invocation_count = len(test_filter.split(","))
                self.assertEqual(invocation_count, len(bazel.splitlines()))
                self.assertEqual(invocation_count, len(cargo.splitlines()))
                self.assertIn(label, bazel)
                self.assertIn(f"-p {package}", cargo)
                if target:
                    self.assertIn(target, cargo)
                self.assertIn(
                    {
                        "nextest": "cargo nextest run -p",
                        "nextest-ci": "cargo nextest run --profile ci -p",
                        "cargo-test": "cargo test -p",
                    }[runner],
                    cargo,
                )
                for selected_filter in filter(None, test_filter.split(",")):
                    # The one selection reaches both dialects. A libtest filter
                    # that matches nothing exits 0, so a name present on one
                    # side only is a silently retired leg.
                    self.assertIn(f"--test_arg={selected_filter}", bazel)
                    self.assertIn(
                        selected_filter if runner == "cargo-test" else f"test({selected_filter})",
                        cargo,
                    )
                for flag in filter(None, flags.split(",")):
                    bazel_spelling, cargo_spellings = flag_dialects[flag]
                    self.assertIn(bazel_spelling, bazel, flag)
                    self.assertTrue(
                        any(spelling in cargo for spelling in cargo_spellings),
                        (flag, cargo),
                    )
                # And a flag the row does not ask for is in neither half.
                for flag, (bazel_spelling, _) in flag_dialects.items():
                    if flag not in flags.split(","):
                        self.assertNotIn(bazel_spelling, bazel, (suite, flag))

        # The rendered arms are gone from the `case`, so there is no second
        # place a selection could be written.
        case_body = script.split("\ncase \"${suite}\" in\n", 1)[1]
        for suite in uniform:
            self.assertNotIn(f"\n  {suite})\n", case_body, suite)

    def test_store_suites_run_the_law_receipt_census_in_both_dialects(self) -> None:
        """pg-store and s3-store census law execution on either runner.

        The census is the FIG-3429 merge gate: a registered law that produced
        no execution receipt fails the job. It must run on BOTH dialects for
        the same reason the suite's selection does — an untrusted event takes
        the Cargo half and would never see a Bazel-only census.
        """
        for suite in ("pg-store", "s3-store"):
            with self.subTest(suite=suite):
                bazel, cargo = store_suite_branches(suite)
                for rendered in (bazel, cargo):
                    self.assertIn(
                        "python3 scripts/check_law_execution_receipts.py",
                        rendered,
                    )

    def test_s3_ci_lane_requires_storage_configuration(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        s3_store_job = workflow_job_block(workflow, "s3-store")

        self.assertNotIn("LASH_S3_ENDPOINT:", s3_store_job)
        self.assertIn('LASH_REQUIRE_S3: "1"', s3_store_job)

        # The require flag is supplied once at job level, so an unavailable
        # service fails instead of skipping for every suite in the job and no
        # step can lose it on its own. A Bazel test spawn inherits nothing from
        # the client environment, so `bazel_test` forwards it by name.
        s3_job_env = yaml.safe_load(workflow)["jobs"]["s3-store"]["env"]
        self.assertEqual("1", str(s3_job_env["LASH_REQUIRE_S3"]))
        self.assertNotIn("LASH_S3_ENDPOINT", s3_job_env)
        # The endpoint follows the port `with-service.sh` chose (the shared
        # S3 service renders it), and every suite in this job runs inside
        # that wrapper.
        wrapper = (ROOT / "scripts" / "ci" / "with-service.sh").read_text(
            encoding="utf-8"
        )
        self.assertIn('lash_s3_test_env "$port"', wrapper)
        s3_service = (ROOT / "scripts" / "ci" / "s3-service.sh").read_text(encoding="utf-8")
        self.assertIn('"LASH_S3_ENDPOINT=http://127.0.0.1:$1"', s3_service)
        for step_name in (
            "Test S3 store conformance",
            "Test attachment blob-store differential",
        ):
            with self.subTest(step=step_name):
                self.assertIn(
                    "bash scripts/ci/with-service.sh s3 --",
                    workflow_step_block(s3_store_job, step_name),
                )
        bazel_helper = STORE_TESTS.read_text(encoding="utf-8").split(
            "bazel_test() {", 1
        )[1].split("\n}", 1)[0]
        self.assertIn("--test_env=LASH_REQUIRE_S3", bazel_helper)

        conformance_bazel, conformance_cargo = store_suite_branches(
            store_suite_for_step(
                workflow_step_block(s3_store_job, "Test S3 store conformance")
            )
        )
        self.assertIn("cargo test -p lash-internal-s3-store --locked", conformance_cargo)
        # The Bazel half runs the generated label set, so a new S3-gated
        # binary joins this job without a hand edit. The generated file is what
        # has to name the crate, and the rendered command is what has to carry
        # every label in it.
        s3_labels = (ROOT / "tools" / "bazel" / "s3_test_labels.txt").read_text(
            encoding="utf-8"
        )
        self.assertIn("//crates/lash-s3-store:", s3_labels)
        for label in s3_labels.split():
            self.assertIn(label, conformance_bazel, label)

        differential_bazel, differential_cargo = store_suite_branches(
            store_suite_for_step(
                workflow_step_block(
                    s3_store_job, "Test attachment blob-store differential"
                )
            )
        )
        for branch in (differential_bazel, differential_cargo):
            self.assertIn("attachment_blob_store_differential_agrees", branch)

    def test_every_declared_lane_reaches_the_pool_graph(self) -> None:
        """Each coverage lane is compiled by the one Bazel feature job.

        The fourteen `Package feature check` legs and the four
        `Runtime feature boundary` legs were a matrix of Cargo commands; they
        are now one job building `//:feature_lanes`. The lane names are still
        the contract, so they are spelled out here rather than read out of the
        table under test.
        """
        lanes = feature_lane_table()
        self.assertEqual(
            {
                "sansio-schema-validation",
                "otel-feature-chain",
                "core-internal-features",
                "language-testing-features",
                "llm-transport-features",
                "store-features",
                "runtime-features",
                "protocol-rlm-testing",
                "remote-protocol-conversions",
                "tool-lashlang-proxies",
                "provider-testing-features",
                "perf-dhat-heap",
                "regress-stable-features",
                "host-features",
            },
            set(lanes),
        )
        for lane, labels in sorted(lanes.items()):
            with self.subTest(lane=lane):
                self.assertTrue(labels, f"lane {lane} compiles nothing")

        job = workflow_job_block(WORKFLOW.read_text(encoding="utf-8"), "feature-lanes")
        self.assertIn("//:feature_lanes", job)
        self.assertIn("//:feature_lane_tests", job)
        self.assertIn(
            "python3 scripts/ci/check_feature_lane_test_floors.py", job
        )

    def test_lash_runtime_default_build_still_runs_and_counts_its_tests(self) -> None:
        """The `default-off-tests` leg survived the move onto the pool.

        It carried two claims: the default build's own suite runs, and its case
        count does not fall. The suite is a lane command in
        `scripts/feature-coverage.toml` (so it is a pool test target); the floor
        is `FEATURE_LANE_TEST_FLOORS`, held by a step of the feature job.
        """
        plan = tomllib.loads(FEATURE_COVERAGE.read_text(encoding="utf-8"))
        runtime = next(
            lane for lane in plan["lane"] if lane["name"] == "runtime-features"
        )
        self.assertIn(
            ["cargo", "test", "-p", "lash-runtime", "--no-default-features", "--locked"],
            runtime["commands"],
        )

        generator = GENERATOR.read_text(encoding="utf-8")
        self.assertIn('("lash-runtime", (), "unit-test"): 130,', generator)
        lanes = LANE_TABLE.read_text(encoding="utf-8")
        marker = "FEATURE_LANE_TEST_FLOORS = "
        floors = json.loads(
            lanes[lanes.index(marker) + len(marker) : lanes.index("\n\nFEATURE_LANES")]
        )
        self.assertEqual([130], sorted(floors.values()))
        self.assertTrue(
            all("crates/lash:" in label for label in floors),
            f"the runtime floor names an unexpected target: {sorted(floors)}",
        )

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
        # Host Application binary stamping belongs to that host's own release.
        self.assertIn("publish_workspace.py --version", release)
        self.assertIn("permissions:\n  contents: read\n  actions: read", release)
        publish = workflow_job_block(release, "publish")
        self.assertIn("permissions:\n      contents: write", publish)

    def test_workspace_tests_build_and_run_without_archive_transport(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        # The workspace suite builds and runs on one runner. This keeps its
        # feature graph and profile while removing multi-gigabyte archive
        # transport and four repeated runner setup paths.
        self.assertNotIn("  test:\n", workflow)
        self.assertIn("  check:\n", workflow)
        self.assertIn("  workspace-tests:\n", workflow)
        self.assertNotIn("  nextest-archive:\n", workflow)
        self.assertNotIn("  test-shard:\n", workflow)
        self.assertNotIn("cargo nextest archive", workflow)
        self.assertNotIn("--archive-file", workflow)
        workspace_tests = workflow_job_block(workflow, "workspace-tests")
        self.assertIn("Install Node for browser projection gates", workspace_tests)
        self.assertIn("node-version: 24", workspace_tests)
        # Untrusted pull requests keep the full workspace build and the store
        # conformance helper example. Trusted events run Bazel instead.
        self.assertIn(
            'cargo build --locked ${LASH_CI_FEATURES} "${build_args[@]}"',
            workspace_tests,
        )
        self.assertIn(
            'build_args=(--workspace "${excludes[@]}"'
            " --example sqlite-await-event-helper)",
            workspace_tests,
        )
        self.assertIn("trusted event must use the Bazel partition", workspace_tests)
        self.assertNotIn("scope=(--package agent-workbench)", workspace_tests)
        self.assertIn(
            "cargo nextest run --profile ci --locked ${LASH_CI_FEATURES}",
            workspace_tests,
        )
        self.assertIn("Logical CPUs: $(nproc)", workspace_tests)
        # --no-fail-fast so one failure never hides the rest (alpha.82 lesson).
        self.assertIn("--no-fail-fast", workspace_tests)

        # Gates that neither warm nor consume the seal cache are sibling jobs, not serial steps
        # behind twelve minutes of compilation.
        # Doctests were removed from the repository by ruling (2026-09-13), so no half of this
        # job runs them on either trust path.
        check_job = workflow_job_block(workflow, "check")
        self.assertNotIn("cargo check --workspace --all-targets --locked", check_job)
        self.assertNotIn("--doc ", check_job)
        # The trusted path is pure Bazel (FIG-3364) and rides the core test
        # invocation: `ui_fixtures` runs every tests/ui/*.stderr fixture
        # through the toolchain rustc directly and, as its validation output,
        # builds `ui__test` (which proves the compile-pass modules still
        # compile), so the seal no longer pays trybuild's nested `cargo check`
        # of the dependency graph or a Bazel client of its own. The Cargo
        # command stays as the untrusted/fork leg, so both spellings are
        # pinned here.
        self.assertIn(
            "cargo test --workspace --locked ${LASH_CI_FEATURES} --test ui",
            check_job,
        )
        self.assertNotIn("bazel ", check_job)
        self.assertNotIn("bazel-shared-cache", check_job)
        self.assertIn(
            "//crates/lash:ui_fixtures", workflow_job_block(workflow, "bazel-tests")
        )
        self.assertNotIn("run-seal-harness", check_job)
        self.assertNotIn("cargo fetch", check_job)
        for foreign in (
            "cargo check -p agent-service --features restate --all-targets --locked",
            "cargo check -p lash-runtime --no-default-features --locked",
        ):
            self.assertNotIn(foreign, check_job)

        # Every gate that moved is pinned to the job that now owns it. Asserting
        # only that check no longer runs them would be satisfied by deleting
        # them outright, which is the way a decomposition silently drops
        # coverage: the split is a scheduling change, so each command has to be
        # somewhere, and named.
        moved_gates = {
            "repo-gates": (
                "python3 scripts/lint_orchestrating_tools.py",
                "python3 scripts/check_test_quarantines.py",
                "bash scripts/test-worktree-gate-env.sh",
                "bash scripts/test-dev-script-process-identity.sh",
            ),
            "feature-lanes": (
                "//:feature_lanes",
                "//:feature_lane_tests",
                "python3 scripts/ci/check_feature_lane_test_floors.py",
            ),
        }
        for job_id, commands in moved_gates.items():
            self.assertIn(f"  {job_id}:\n", workflow)
            block = workflow_job_block(workflow, job_id)
            for command in commands:
                with self.subTest(job=job_id, command=command):
                    self.assertIn(command, block)

        # The hand-enumerated script self-tests moved as a block; the discovery
        # test above proves CI runs every one, this proves they all live in the
        # job that took them.
        repo_gates = workflow_job_block(workflow, "repo-gates")
        discovered = sorted(path.name for path in (ROOT / "scripts").glob("test_*.py"))
        self.assertGreater(len(discovered), 5, "the self-test discovery found nothing")
        for name in discovered:
            with self.subTest(self_test=name):
                self.assertIn(f"python3 scripts/{name}", repo_gates)

        # The `dependency-boundary` leg reads the resolved graph and compiles
        # nothing, so it moved to the Python-speed gates rather than onto the
        # pool with the compile legs.
        self.assertIn(
            "cargo tree -p lash-runtime -e normal $resolution --locked",
            repo_gates,
        )

    def test_one_ci_run_per_head_branch_whatever_the_trigger(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        # A `pull_request` run carries refs/pull/<n>/merge and a manual recovery
        # dispatch of the same commit carries refs/heads/<branch>: keying the
        # concurrency group on `github.ref` put them in different groups and ran
        # the whole board twice over one tree. Same-repo pull requests and
        # dispatches share the head-branch key, which is what makes the dispatch
        # supersede the pull-request run instead of doubling it.
        group = workflow[workflow.index("concurrency:") : workflow.index("permissions:")]
        self.assertIn("|| github.head_ref", group)
        self.assertIn("|| github.ref_name", group)

        # Full-profile release certification uses its own dispatch group, so the
        # exact-SHA evidence release.yml requires is never cancelled by another
        # run. The key carries a colon, which `git check-ref-format` forbids in a
        # ref name: that is what makes it unforgeable by any branch, in this
        # repository or a fork, rather than merely unlikely to collide.
        self.assertIn(
            "(github.event_name == 'workflow_dispatch' && github.ref_name == 'main')"
            " && 'trunk:dispatch'",
            group,
        )
        # A fork's branch name is not this repository's namespace, so a fork PR is
        # keyed by PR number — also colon-guarded, so no branch can imitate it.
        self.assertIn(
            "github.event.pull_request.head.repo.full_name != github.repository",
            group,
        )
        self.assertIn("format('fork-pr:{0}', github.event.pull_request.number)", group)
        for key in ("trunk:dispatch", "fork-pr:{0}"):
            with self.subTest(key=key):
                self.assertIn(":", key, "an unforgeable key needs the forbidden colon")

        self.assertIn(
            "cancel-in-progress: ${{ github.event_name == 'pull_request' || "
            "(github.event_name == 'workflow_dispatch' && github.ref_name != 'main') }}",
            group,
        )

    def test_required_checks_are_delivered_to_merge_queue_entries(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        # A merge-queue entry runs from its own gh-readonly-queue ref and gets no
        # checks at all unless the workflow subscribes to `merge_group`; without
        # this the queue stalls on checks that never report.
        self.assertIn("  merge_group:\n", workflow)

        # Cache warming is not a queue gate: a cache written from a queue ref is
        # scoped to that ref and discarded with it, so warming it there would
        # only add the longest job on the board to every queue entry.
        release_cache_workflow = RELEASE_CACHE_WORKFLOW.read_text(encoding="utf-8")
        self.assertNotIn("  linux-release-cache:\n", workflow)
        # The warmer is manual like every other heavy build: nothing in this
        # repository runs automatically on a push to main.
        self.assertNotIn("  push:\n", release_cache_workflow)
        self.assertIn("  workflow_dispatch:\n", release_cache_workflow)
        self.assertIn("group: linux-release-cache-main", release_cache_workflow)
        self.assertIn("cancel-in-progress: false", release_cache_workflow)
        self.assertIn("if: github.ref == 'refs/heads/main'", release_cache_workflow)

        # A queue head carries a whole PR — often several commits — on top of the
        # base the queue chose, so HEAD~1 checks only its last commit: the gate
        # passes vacuously when the change is earlier and fails falsely when the
        # bump is. The queue states its base; the gate uses it.
        lint = workflow_job_block(workflow, "lint")
        bumps = workflow_step_block(lint, "Check versioned surface bumps")
        self.assertIn(
            "MERGE_GROUP_BASE_SHA: ${{ github.event.merge_group.base_sha }}", bumps
        )
        self.assertIn('elif [[ "$GITHUB_EVENT_NAME" == "merge_group" ]]; then', bumps)
        self.assertIn('base="$MERGE_GROUP_BASE_SHA"', bumps)
        # An empty base would silently become `--base ""`, so it fails loudly.
        self.assertIn(
            '[ -n "$base" ] || { echo "merge_group event carried no base_sha"; exit 1; }',
            bumps,
        )

    def test_manual_dispatch_explains_skipped_version_bump_gate(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        for trigger in (
            "  workflow_dispatch:\n",
            "  pull_request:\n",
            "  merge_group:\n",
        ):
            self.assertIn(trigger, workflow)
        self.assertNotIn("\n  push:\n", workflow)

        lint = workflow_job_block(workflow, "lint")
        bumps = workflow_step_block(lint, "Check versioned surface bumps")
        self.assertIn("if: github.event_name != 'workflow_dispatch'", bumps)
        manual = workflow_step_block(lint, "Explain skipped versioned surface bump gate")
        self.assertIn("if: github.event_name == 'workflow_dispatch'", manual)
        self.assertIn("bash scripts/ci/report-manual-dispatch-skips.sh", manual)

        script = ROOT / "scripts" / "ci" / "report-manual-dispatch-skips.sh"
        with tempfile.TemporaryDirectory() as directory:
            summary = pathlib.Path(directory) / "summary.md"
            result = subprocess.run(
                ["bash", str(script)],
                env=dict(os.environ, GITHUB_STEP_SUMMARY=str(summary)),
                capture_output=True,
                text=True,
            )
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertIn("::warning", result.stdout)
            self.assertIn("Check versioned surface bumps", result.stdout)
            self.assertIn("green manual run does not prove it passed", result.stdout)
            self.assertIn("Check the Test262 outcome ratchet", result.stdout)
            summary_text = summary.read_text(encoding="utf-8")
            self.assertIn("Manual CI run is incomplete", summary_text)
            self.assertIn("Check versioned surface bumps", summary_text)
            self.assertIn("green manual run does not prove that gate passed", summary_text)
            self.assertIn("Check the Test262 outcome ratchet", summary_text)

    def test_the_transcript_gate_can_read_a_queued_pull_request(self) -> None:
        gate = (ROOT / "scripts" / "check-transcript-diff.py").read_text(
            encoding="utf-8"
        )

        # In the queue there is no pull request in the payload and the ref is
        # nobody's head branch, so the by-head lookup finds nothing. The queue
        # ref names the PR; the gate has to use it or it fails a justified change
        # at the last gate before merge.
        self.assertIn("gh-readonly-queue/[^/]+/pr-(?P<number>\\d+)-[0-9a-f]+", gate)
        self.assertIn("def queried_pull_request_body_by_number(", gate)

    def test_every_shared_debug_cache_reader_resolves_the_writer_rustflags(
        self,
    ) -> None:
        workflow = yaml.safe_load(WORKFLOW.read_text(encoding="utf-8"))
        # RUSTFLAGS is part of cargo's per-unit fingerprint. When the flag was
        # appended by a step, the five jobs that restore `linux-debug` without
        # that step matched the cache key, logged "cache restored", and then
        # cold-built the entire graph. The flag is a property of the workflow
        # now, so nothing about a job's step list can move it.
        self.assertEqual((workflow.get("env") or {}).get("RUSTFLAGS"), MOLD_RUSTFLAGS)

        for job_id, job in workflow["jobs"].items():
            steps = job.get("steps") or []
            if not any(
                (step.get("with") or {}).get("shared-key") == "linux-debug"
                for step in steps
            ):
                continue
            with self.subTest(job=job_id):
                # No job-scoped or step-scoped override may reintroduce the
                # split this fixed.
                self.assertNotIn("RUSTFLAGS", job.get("env") or {})
                for step in steps:
                    self.assertNotIn("RUSTFLAGS", step.get("env") or {})
                # The flag names a linker that has to exist: a job with the
                # fingerprint but without the binary cannot link at all.
                self.assertTrue(
                    any(
                        "setup-mold" in (step.get("uses") or "")
                        for step in steps
                    ),
                    f"{job_id} resolves the mold RUSTFLAGS without installing mold",
                )

    def test_linux_release_cache_stays_mold_free_across_workflows(self) -> None:
        release_cache_workflow = yaml.safe_load(
            RELEASE_CACHE_WORKFLOW.read_text(encoding="utf-8")
        )
        perf = PERF_WORKFLOW.read_text(encoding="utf-8")
        release = RELEASE_WORKFLOW.read_text(encoding="utf-8")

        # perf.yml and release.yml restore `linux-release` and link with the
        # default linker. The one job that writes that cache has to opt out of
        # the workflow-level flag, or those two workflows rebuild everything
        # they restore. Empty and unset are the same value to cargo.
        self.assertEqual(
            release_cache_workflow["env"]["RUSTFLAGS"], ""
        )
        for name, text in (("perf.yml", perf), ("release.yml", release)):
            with self.subTest(workflow=name):
                self.assertNotIn("mold", text)
                self.assertNotIn("RUSTFLAGS", text)

    def test_workspace_test_cache_keeps_the_test_build_key(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")
        workspace_tests = workflow_job_block(workflow, "workspace-tests")

        # Keep the dependency cache used by the former archive producer. It is
        # smaller than linux-debug and is already keyed for this feature graph.
        cache_action = (
            "uses: Swatinem/rust-cache@"
            "6323deb102c322ba6fcbdcafc7e3dddab59af2b6 # v2.9.2"
        )
        self.assertIn(cache_action, workspace_tests)
        self.assertIn("shared-key: linux-tests", workspace_tests)
        self.assertIn("cache-targets: false", workspace_tests)
        self.assertIn("actions/cache/restore@55cc8345863c7cc4c66a329aec7e433d2d1c52a9", workspace_tests)
        # Restore-only: cache-warm.yml writes both keys on main, and a save
        # from a pull request's ref is readable by nothing else.
        self.assertNotIn("actions/cache/save@", workspace_tests)
        self.assertIn("save-if: false", workspace_tests)
        warm = yaml.safe_load(
            (ROOT / ".github" / "workflows" / "cache-warm.yml").read_text(encoding="utf-8")
        )["jobs"]["warm-workspace-tests"]
        ci_steps = {
            step["name"]: step
            for step in yaml.safe_load(workflow)["jobs"]["workspace-tests"]["steps"]
        }
        warm_steps = {step["name"]: step for step in warm["steps"]}
        restore = ci_steps["Restore workspace test build outputs"]["with"]
        writer = warm_steps["Restore and save workspace test build outputs"]["with"]
        for field in ("path", "key", "restore-keys"):
            self.assertEqual(restore[field], writer[field], field)
        self.assertEqual(
            {k: v for k, v in ci_steps["Restore cargo cache"]["with"].items() if k != "save-if"},
            warm_steps["Restore cargo cache"]["with"],
        )
        self.assertIn("workspace-tests-v2-${{ runner.os }}-${{ runner.arch }}", workspace_tests)
        self.assertIn("rustc -vV", workspace_tests)
        self.assertIn("outputs.compiler", workspace_tests)
        self.assertIn("outputs.settings", workspace_tests)
        self.assertIn("outputs.source", workspace_tests)
        self.assertIn("ci_workspace_cache.py restore", workspace_tests)
        self.assertIn("ci_workspace_cache.py snapshot", workspace_tests)
        self.assertIn("target/workspace-test-cache/source-mtimes.json", workspace_tests)
        self.assertIn("restore-keys:", workspace_tests)
        for path in (
            "target/debug/.fingerprint",
            "target/debug/build",
            "target/debug/deps",
            "target/debug/examples",
        ):
            self.assertIn(path, workspace_tests)
        self.assertNotIn("target/debug/incremental", workspace_tests)

    def test_ci_has_no_staging_or_automatic_release_path(self) -> None:
        workflow = WORKFLOW.read_text(encoding="utf-8")

        self.assertNotIn("staging", workflow)
        self.assertNotIn("prepare-release", workflow)
        self.assertNotIn("git tag", workflow)
        self.assertNotIn("gh workflow run release.yml", workflow)


class ReleaseConfidenceTests(unittest.TestCase):
    def setUp(self):
        from unittest.mock import patch
        workflow = yaml.safe_load(RELEASE_WORKFLOW.read_text())
        script = next(step["run"] for step in workflow["jobs"]["prepare-release"]["steps"]
                      if step["name"] == "Resolve and validate release commit")
        source = script.split("python3 - <<'CONFIDENCE_PY'\n", 1)[1].split("\nCONFIDENCE_PY", 1)[0]
        namespace = {"__name__": "contract_test"}
        exec(compile(source, str(RELEASE_WORKFLOW), "exec"), namespace)
        self.check = namespace["check_confidence"]
        self.now = dt.datetime(2026, 9, 6, tzinfo=dt.timezone.utc)
        self.run = {"databaseId": 123, "headSha": "abc", "url": "https://example.test/runs/123",
                    "status": "completed", "conclusion": "success"}
        self.completed = "2026-09-05T00:00:00Z"
        self.output = self.enterContext(patch("subprocess.check_output"))
        self.ancestry = self.enterContext(patch("subprocess.run"))
        self.ancestry.return_value.returncode = 0
        self.enterContext(patch("builtins.print"))
        self.refresh()

    def refresh(self, runs=None):
        self.output.side_effect = [json.dumps([self.run] if runs is None else runs),
                                  json.dumps([{"jobs": [{"completed_at": self.completed}]}])]

    def test_fresh_green_ancestor_or_equal_passes(self):
        for target in ("abc", "descendant"):
            self.refresh()
            self.check(target, "", self.now)
            self.ancestry.assert_called_with(["git", "merge-base", "--is-ancestor", "abc", target], check=False)
        command = self.output.call_args_list[0].args[0]
        self.assertIn("schedule", command)
        self.assertIn("main", command)
        self.assertEqual("1", command[command.index("--limit") + 1])

    def test_latest_red_refuses_without_falling_back(self):
        self.run["conclusion"] = "failure"
        self.refresh()
        with self.assertRaises(SystemExit) as error:
            self.check("target", "", self.now)
        for detail in ("release refused", "https://example.test/runs/123", "abc", "1.000 days", "failure"):
            self.assertIn(detail, str(error.exception))

    def test_eight_day_boundary_and_stale_future_or_missing_completion(self):
        self.completed = "2026-08-29T00:00:00Z"
        self.refresh()
        self.check("target", "", self.now)
        for completed in ("2026-08-28T23:59:59Z", "2026-09-07T00:00:00Z", None):
            with self.subTest(completed=completed):
                self.completed = completed
                self.refresh()
                with self.assertRaisesRegex(SystemExit, "older than 8 days"):
                    self.check("target", "", self.now)

    def test_missing_weekly_refuses_with_diagnostics(self):
        self.refresh([])
        with self.assertRaisesRegex(SystemExit, "run URL=unavailable; head SHA=unavailable; age=unknown; conclusion=missing"):
            self.check("target", "", self.now)

    def test_nonancestor_refuses(self):
        self.ancestry.return_value.returncode = 1
        with self.assertRaisesRegex(SystemExit, "not an ancestor"):
            self.check("target", "", self.now)

    def test_running_weekly_refuses(self):
        self.run.update(status="in_progress", conclusion="")
        self.refresh()
        with self.assertRaisesRegex(SystemExit, "not green"):
            self.check("target", "", self.now)

    def test_override_requires_nonblank_reason_and_logs(self):
        from unittest.mock import patch
        with patch("builtins.print") as output:
            self.check("target", "emergency release\nwith review", self.now)
            self.assertIn("::warning::CONFIDENCE RELEASE OVERRIDE", output.call_args.args[0])
            self.assertIn("emergency release", output.call_args.args[0])
            self.assertNotIn("\n", output.call_args.args[0])
        self.output.assert_not_called()
        self.refresh([])
        with self.assertRaises(SystemExit):
            self.check("target", " \n ", self.now)

    def test_api_failure_is_not_silently_bypassed(self):
        self.output.side_effect = subprocess.CalledProcessError(1, "gh")
        with self.assertRaises(subprocess.CalledProcessError):
            self.check("target", "", self.now)


class MutationRequestTests(unittest.TestCase):
    def test_mutation_lives_on_weekly_confidence_not_a_pr_workflow(self):
        self.assertFalse((ROOT / ".github/workflows/mutation.yml").exists())
        ci = yaml.safe_load(WORKFLOW.read_text())
        self.assertNotIn("mutation", ci["jobs"]["ci-conclusion"]["needs"])
        confidence = yaml.safe_load(CONFIDENCE_WORKFLOW.read_text())
        for job in (
            "confidence-mutation-core",
            "confidence-mutation-sim",
            "confidence-mutation-packages",
        ):
            self.assertIn(job, confidence["jobs"])
        gate = GATE.read_text()
        request = gate.split('  # Every mutation in the targeted files, using the same weekly runner.', 1)[1].split('fi', 1)[0]
        self.assertIn("run_area_targeted_mutation_evidence", request)
        plan = subprocess.run(["bash", str(GATE), "--dry-run", "mutation"], text=True, capture_output=True)
        self.assertEqual(0, plan.returncode, plan.stderr)
        self.assertIn("effect_replay_driver", plan.stdout)
        self.assertIn("commit_admission", plan.stdout)
        self.assertIn("Mutation scope: targeted", plan.stdout)


if __name__ == "__main__":
    unittest.main()
