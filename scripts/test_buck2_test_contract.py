#!/usr/bin/env python3
"""Contract tests for trusted Buck2 CI and untrusted Cargo fallback paths."""

from __future__ import annotations

import hashlib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile
import unittest

import yaml


ROOT = Path(__file__).resolve().parents[1]


def workflow(name: str) -> dict:
    return yaml.safe_load((ROOT / ".github/workflows" / name).read_text(encoding="utf-8"))


def step(job: dict, name: str) -> dict:
    return next(item for item in job["steps"] if item.get("name") == name)


# One job per build of the store package (scripts/ci_plan.py owns the tuple).
POSTGRES_STORE_JOBS = ("postgres-store", "postgres-store-synthetic-next")


def store_suites(job: dict) -> list[str]:
    """The suites a job's steps hand to store-tests.sh, in step order."""
    return [
        item["run"].split("scripts/ci/store-tests.sh", 1)[1].split()[0]
        for item in job["steps"]
        if "scripts/ci/store-tests.sh" in item.get("run", "")
    ]


def suite_labels(suite: str) -> list[str]:
    return subprocess.run(
        ["bash", str(ROOT / "scripts/ci/store-tests.sh"), "--labels", suite],
        cwd=ROOT, check=True, capture_output=True, text=True,
    ).stdout.split()


def shared_action() -> dict:
    return yaml.safe_load(
        (ROOT / ".github/actions/buck2-shared-cache/action.yml").read_text(
            encoding="utf-8"
        )
    )["runs"]


class SharedActionTests(unittest.TestCase):
    def setUp(self) -> None:
        self.text = (ROOT / ".github/actions/buck2-shared-cache/action.yml").read_text()

    def test_requires_and_masks_the_existing_pool_secrets(self) -> None:
        for name in (
            "CACHE_ENDPOINT",
            "CACHE_INSTANCE",
            "KILN_EXECUTOR_RUNTIME",
            "CACHE_CA",
            "CACHE_CERT",
            "CACHE_KEY",
        ):
            self.assertIn(name, self.text)
        self.assertIn("::add-mask::${endpoint}", self.text)
        self.assertIn("::add-mask::${instance}", self.text)
        self.assertIn("::add-mask::${runtime}", self.text)
        self.assertIn('mask_multiline CACHE_CA', self.text)
        self.assertIn('mask_multiline CACHE_CERT', self.text)
        self.assertIn('mask_multiline CACHE_KEY', self.text)

    def test_installs_only_the_repository_pinned_official_client(self) -> None:
        self.assertIn("python3 tools/buck2/bootstrap.py", self.text)
        self.assertIn("hashFiles('tools/buck2/*pins.json'", self.text)
        self.assertNotIn("client-Cargo.lock", self.text)
        self.assertNotIn("patches/**", self.text)
        self.assertNotIn("bazel", self.text.lower())

    def test_tool_cache_uses_exact_bootstrap_inputs_and_materialized_tools(self) -> None:
        runs = shared_action()
        key_script = step(runs, "Resolve Buck2 tool cache key")["run"]
        key_inputs = re.search(r"hashFiles\(([^)]*)\)", key_script)
        self.assertIsNotNone(key_inputs)
        self.assertEqual(
            {
                "tools/buck2/*pins.json",
                "tools/buck2/*lock.json",
                "tools/buck2/*bootstrap*.py",
                "tools/buck2/prelude_overlay.py",
                "tools/buck2/reindeer.toml",
                "tools/buck2/fixups/**",
                "third-party/Cargo.toml",
                "third-party/Cargo.lock",
            },
            set(re.findall(r"'([^']+)'", key_inputs.group(1))),
        )
        self.assertIn("python_abi=", key_script)
        self.assertIn("-${python_abi}-${tool_digest}", key_script)
        restore = step(runs, "Restore pinned Buck2 tools")["with"]
        expected_paths = {
            ".buck2/bin",
            ".buck2/native",
            ".buck2/prelude",
            ".buck2/test-runner",
            "tools/buck2/bin",
            "tools/buck2/toolchains/rust-files",
            "vendor",
        }
        self.assertEqual(expected_paths, set(restore["path"].splitlines()))
        self.assertNotIn("restore-keys", restore)
        self.assertNotIn(".buck2/downloads", restore["path"])

        warm = workflow("cache-warm.yml")["jobs"]["warm-buck2"]
        save = step(warm, "Save pinned Buck2 tools")["with"]
        self.assertEqual(expected_paths, set(save["path"].splitlines()))

    def test_writes_authenticated_remote_configuration_without_fallback(self) -> None:
        for line in (
            "engine_address = ${POOL_ADDRESS}",
            "action_cache_address = ${POOL_ADDRESS}",
            "cas_address = ${POOL_ADDRESS}",
            "tls_ca_certs = $RUNNER_TEMP/build-cache/ca.crt",
            "tls_client_cert = $RUNNER_TEMP/build-cache/client.pem",
            "instance_name = ${POOL_INSTANCE}",
            "execution_concurrency_limit = 32",
            "executor_runtime = ${POOL_RUNTIME}",
        ):
            self.assertIn(line, self.text)
        self.assertIn('^grpcs://', self.text)
        self.assertNotIn('^grpcs?://', self.text)

    def test_pool_validation_fails_closed_and_normalizes_secret_newlines(self) -> None:
        verify = step(shared_action(), "Verify shared pool configuration")["run"]
        names = (
            "CACHE_ENDPOINT",
            "CACHE_INSTANCE",
            "KILN_EXECUTOR_RUNTIME",
            "CACHE_CA",
            "CACHE_CERT",
            "CACHE_KEY",
        )
        supplied = {name: f"value-of-{name}" for name in names}
        supplied["CACHE_ENDPOINT"] = "grpcs://cache.example:8443"

        def run(environment: dict[str, str]) -> subprocess.CompletedProcess[str]:
            with tempfile.TemporaryDirectory() as directory:
                output = Path(directory) / "output"
                completed = subprocess.run(
                    ["bash", "-c", verify],
                    cwd=ROOT,
                    env=os.environ | environment | {"GITHUB_OUTPUT": str(output)},
                    capture_output=True,
                    text=True,
                )
                if output.exists():
                    completed.stdout += output.read_text(encoding="utf-8")
                return completed

        complete = run(supplied)
        self.assertEqual(0, complete.returncode, complete.stderr)
        self.assertIn("address=cache.example:8443", complete.stdout)
        normalized = run(
            supplied
            | {
                "CACHE_ENDPOINT": "grpcs://cache.example:8443\n",
                "KILN_EXECUTOR_RUNTIME": "kiln-runtime-sha256-example\n",
            }
        )
        self.assertEqual(0, normalized.returncode, normalized.stderr)
        self.assertIn("runtime=kiln-runtime-sha256-example\n", normalized.stdout)
        for name in names:
            with self.subTest(missing=name):
                missing = run(supplied | {name: ""})
                self.assertEqual(1, missing.returncode)
                self.assertIn(name, missing.stderr)
        for endpoint in (
            "cache.example:8443",
            "grpcs://cache.example",
            "grpc://cache.example:8443",
            "https://cache.example:8443",
        ):
            with self.subTest(endpoint=endpoint):
                malformed = run(supplied | {"CACHE_ENDPOINT": endpoint})
                self.assertEqual(1, malformed.returncode)
                self.assertIn("CACHE_ENDPOINT", malformed.stderr)

    def configure_client(self, environment: dict[str, str]) -> tuple[str, set[str]]:
        configure = step(shared_action(), "Configure authenticated Buck2 client")["run"]
        with tempfile.TemporaryDirectory() as directory:
            temp = Path(directory)
            completed = subprocess.run(
                ["bash", "-c", configure],
                cwd=temp,
                env=os.environ
                | {
                    "CACHE_CA": "ca",
                    "CACHE_CERT": "cert",
                    "CACHE_KEY": "key",
                    "POOL_ADDRESS": "cache.example:8443",
                    "POOL_INSTANCE": "kiln",
                    "POOL_RUNTIME": "kiln-runtime-sha256-example",
                    "RUNNER_TEMP": str(temp / "runner-temp"),
                    "GITHUB_WORKFLOW": "CI",
                    "GITHUB_JOB": "build",
                    "GITHUB_ACTOR": "octocat",
                }
                | environment,
                capture_output=True,
                text=True,
            )
            self.assertEqual(0, completed.returncode, completed.stderr)
            return (
                (temp / ".buckconfig.local").read_text(encoding="utf-8"),
                {path.name for path in temp.iterdir()},
            )

    def test_writes_the_pool_tag_identity_header(self) -> None:
        config, _ = self.configure_client(
            {
                "GITHUB_WORKFLOW": "CI",
                "GITHUB_JOB": "Feature lanes (4/4)",
                "GITHUB_ACTOR": "octocat",
            }
        )
        self.assertIn(
            "http_headers = baggage: "
            "enduser.id=repo:lash|lane:CI%252FFeature%2520lanes%2520%25284%252F4%2529"
            "|user:octocat|ci\n",
            config,
        )

    def test_tag_values_escape_every_baggage_and_header_separator(self) -> None:
        config, _ = self.configure_client({"GITHUB_ACTOR": "a%b=c;d,e|f:g h"})
        self.assertIn(
            "user:a%2525b%253Dc%253Bd%252Ce%257Cf%253Ag%2520h|ci", config
        )
        header = next(
            line
            for line in config.splitlines()
            if line.startswith("http_headers = ")
        )
        value = header.split("enduser.id=", 1)[1]
        self.assertNotRegex(value, r"[\x00-\x20\x7f,;]|%(?!25)")

    def test_tag_values_expand_nothing_in_the_heredoc(self) -> None:
        marker = "injected-by-tag"
        config, names = self.configure_client(
            {"GITHUB_ACTOR": f"$(touch {marker})`touch {marker}2`"}
        )
        self.assertNotIn(marker, names)
        self.assertNotIn(f"{marker}2", names)
        self.assertIn(
            "user:%2524%2528touch%2520injected-by-tag%2529"
            "%2560touch%2520injected-by-tag2%2560|ci",
            config,
        )

    def test_repository_commits_no_pool_deployment_facts(self) -> None:
        sources = []
        for path in sorted((ROOT / ".github").rglob("*")):
            if path.is_file():
                sources.append((path.relative_to(ROOT), path.read_text(errors="replace")))
        patterns = (
            (r"\b(?!127\.|0\.0\.0\.0)\d{1,3}(?:\.\d{1,3}){3}\b", "IP address"),
            (r"kiln-runtime-sha256-[0-9a-f]{16,}", "executor fingerprint"),
            (r"/home/[a-z]+/", "home directory"),
        )
        for path, text in sources:
            for pattern, description in patterns:
                self.assertIsNone(
                    re.search(pattern, text),
                    f"{path} contains a deployment {description}",
                )


class WorkflowTests(unittest.TestCase):
    def test_cross_backend_differential_stays_in_remote_cacheable_partition(self) -> None:
        inventory = json.loads(
            (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
        )
        target = next(
            target
            for package in inventory["packages"]
            if package["package"] == "lash-sim"
            for target in package["targets"]
            if target.get("cargo") == "cross_backend_store_differential"
        )
        self.assertEqual(target["tags"], [])
        self.assertNotIn("cargo_only", target)
        self.assertIn(target["label"], inventory["workspace_test_targets"])

    @classmethod
    def setUpClass(cls) -> None:
        cls.ci_text = (ROOT / ".github/workflows/ci.yml").read_text()
        cls.ci = workflow("ci.yml")

    def test_filtered_selection_helper_is_in_every_remote_test_bundle(self) -> None:
        rules = (ROOT / "tools/buck2/test_rules.bzl").read_text()
        package = (ROOT / "tools/buck2/BUCK").read_text()
        single = (ROOT / "tools/buck2/test_xml_runner.sh").read_text()
        batch = (ROOT / "tools/buck2/test_batch_runner.sh").read_text()
        self.assertIn('"libtest_selection.py": ctx.attrs.libtest_selection', rules)
        self.assertIn('libtest_selection = "libtest_selection.py"', package)
        self.assertIn('libtest_selection.py" runner', single)
        self.assertIn("--selection-argv-count", single)
        self.assertIn('"--lash-libtest-args"', rules)
        self.assertIn('shard_env["LASH_TEST_EXECUTION_PREFIX_ARG_COUNT"]', rules)
        self.assertIn('"$libtest_selection" batch-members', batch)

    def test_trust_decision_and_required_partition_ids_are_buck2_owned(self) -> None:
        outputs = self.ci["jobs"]["plan"]["outputs"]
        self.assertIn("buck2_trusted", outputs)
        self.assertNotIn("bazel_trusted", outputs)
        for job in ("buck2-tests", "buck2-tests-tail", "feature-lanes"):
            self.assertIn(job, self.ci["jobs"])
            self.assertIn("buck2_trusted", str(self.ci["jobs"][job]["if"]))

    def test_trusted_tests_use_driver_reports_and_event_logs(self) -> None:
        for job_name, step_name in (
            ("buck2-tests", "Test the workspace core suite with shared cache"),
            ("buck2-tests-tail", "Test the workspace tail suite with shared cache"),
            ("feature-lanes", "Run the executable feature lanes"),
        ):
            run = step(self.ci["jobs"][job_name], step_name)["run"]
            self.assertIn("scripts/ci/buck2-test.sh", run)
        self.assertIn("--event-log", self.ci_text)
        self.assertIn("--build-report", self.ci_text)
        self.assertNotIn("buck2 --output_user_root", self.ci_text)
        self.assertNotIn("BUCK2_SHARED_CACHE_FLAGS", self.ci_text)

    def test_feature_compile_clippy_and_schema_use_their_matching_operations(self) -> None:
        feature = step(self.ci["jobs"]["feature-lanes"], "Compile every feature lane")["run"]
        self.assertIn("hermetic-build.sh check", feature)
        self.assertNotIn("hermetic-build.sh build", feature)
        self.assertIn("//:feature_lane_compile", feature)
        lint = step(
            self.ci["jobs"]["lint"],
            "Clippy (workspace, all targets, shared cache)",
        )["run"]
        self.assertIn("hermetic-build.sh clippy", lint)
        self.assertIn("//:workspace_clippy", lint)
        self.assertIn("hermetic-build.sh build", lint)
        self.assertIn("//:schema_checks", lint)

    def test_untrusted_workspace_and_lint_paths_stay_cargo(self) -> None:
        workspace = self.ci["jobs"]["workspace-tests"]
        self.assertIn("buck2_trusted != 'true'", str(workspace["if"]))
        self.assertIn("cargo nextest", str(workspace))
        lint = self.ci["jobs"]["lint"]
        cargo = step(lint, "Clippy (workspace, all targets)")
        self.assertIn("buck2_trusted != 'true'", str(cargo["if"]))
        self.assertIn("cargo clippy --workspace --all-targets", cargo["run"])

    def test_live_store_tests_compile_remotely_but_execute_locally_uncached(self) -> None:
        text = (ROOT / "scripts/ci/store-tests.sh").read_text()
        self.assertIn('"${HERMETIC_BUILD:-scripts/hermetic-build.sh}" test', text)
        self.assertIn("--local-test-execution", text)
        self.assertIn("--no-test-cache", text)
        self.assertIn('inventory["service_test_targets"]', text)
        self.assertIn("cargo test -p lash-internal-postgres-store", text)
        self.assertNotRegex(text, r"\bbazel\b")
        for job_name in (*POSTGRES_STORE_JOBS, "s3-store"):
            job = self.ci["jobs"][job_name]
            upload = next(
                item for item in job["steps"]
                if "Buck2 reports and event logs" in item.get("name", "")
            )
            self.assertIn("store-test-results", upload["with"]["path"])

    def test_cache_warm_is_the_only_actions_cache_writer_for_buck2_tools(self) -> None:
        writers = []
        for path in (ROOT / ".github/workflows").glob("*.yml"):
            text = path.read_text()
            if "actions/cache/save@" in text and ".buck2/bin" in text:
                writers.append(path.name)
        self.assertEqual(writers, ["cache-warm.yml"])
        parsed = yaml.load(
            (ROOT / ".github/workflows/cache-warm.yml").read_text(),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual({"main"}, set(parsed["on"]["push"]["branches"]))
        self.assertIn("schedule", parsed["on"])
        warm = parsed["jobs"]["warm-buck2"]
        self.assertIn("scripts/hermetic-build.sh clippy", str(warm))
        self.assertNotIn("scripts/ci/buck2-test.sh", str(warm))
        self.assertEqual("false", parsed["concurrency"]["cancel-in-progress"])
        configure = step(warm, "Configure Buck2 shared cache")
        self.assertEqual("cache", configure["id"])
        save = step(warm, "Save pinned Buck2 tools")
        self.assertTrue(save["uses"].startswith("actions/cache/save@"))
        self.assertEqual(
            "${{ steps.cache.outputs.tool-cache-key }}", save["with"]["key"]
        )
        self.assertIn("always()", save["if"])
        self.assertIn("steps.cache.outcome == 'success'", save["if"])
        self.assertIn("github.ref == 'refs/heads/main'", save["if"])
        self.assertIn("steps.cache.outputs.tool-cache-hit != 'true'", save["if"])

    def test_warmer_covers_the_check_graphs_agents_and_ci_request(self) -> None:
        warm = workflow("cache-warm.yml")["jobs"]["warm-buck2"]
        run = step(warm, "Warm check actions")["run"]
        inventory = json.loads(
            (ROOT / "tools/buck2/target-inventory.json").read_text()
        )
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            driver = root / "scripts/hermetic-build.sh"
            driver.write_text("#!/bin/bash\nprintf '%s\\n' \"$@\" > argv\n")
            driver.chmod(0o755)
            result = subprocess.run(["bash", "-c", run], cwd=root, capture_output=True, text=True)
            self.assertEqual(0, result.returncode, result.stderr)
            argv = (root / "argv").read_text().splitlines()
        self.assertEqual("check", argv[0])
        labels = {arg for arg in argv if arg.startswith("//")}
        # The two generated aggregates expand to the `[check]` graph: the
        # workspace's default resolution plus every `__fv_` lane variant.
        self.assertEqual({"//:workspace_check", "//:feature_lane_compile"}, labels)
        self.assertTrue(inventory["workspace_check_targets"])
        self.assertTrue(inventory["feature_lane_check_targets"])

    def test_push_static_checks_are_a_separate_visible_job(self) -> None:
        parsed = workflow("cache-warm.yml")
        static = parsed["jobs"]["static-checks"]
        # A sibling of the warm, not a step of it: a red gate fails the
        # workflow without blocking or cancelling the cache fill.
        self.assertNotIn("needs", static)
        self.assertNotIn("static-checks", str(parsed["jobs"]["warm-buck2"]))
        self.assertEqual("github.event_name == 'push'", static["if"])
        text = yaml.dump(static)
        for needle in (
            "//:schema_checks",
            # facade_completeness is an external-runner test rule: building it
            # only renders rustdoc, the check executes under `buck2 test`.
            "buck2-test.sh",
            "//crates/lash:facade_completeness",
            "scripts/ci/repository-gates.sh",
            "hermetic-build.sh check",
            "//:feature_lane_compile",
        ):
            self.assertIn(needle, text)
        self.assertNotIn("continue-on-error", text)
        cleanup = step(static, "Remove client credentials")
        self.assertEqual("always()", cleanup["if"])

    def test_warmer_builds_the_partition_and_worker_binaries_without_tests(self) -> None:
        warm = workflow("cache-warm.yml")["jobs"]["warm-buck2"]
        run = step(warm, "Warm test binaries")["run"]
        inventory = json.loads((ROOT / "tools/buck2/target-inventory.json").read_text())
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "scripts").mkdir()
            driver = root / "scripts/hermetic-build.sh"
            driver.write_text("#!/bin/bash\nprintf '%s\\n' \"$@\" > argv\n")
            driver.chmod(0o755)
            result = subprocess.run(["bash", "-c", run], cwd=root, capture_output=True, text=True)
            self.assertEqual(0, result.returncode, result.stderr)
            argv = (root / "argv").read_text().splitlines()
        self.assertEqual("build", argv[0])
        labels = {arg for arg in argv if arg.startswith("//")}
        expected = {
            "//:workspace_tests",
            "//crates/lash:ui_fixtures",
            "//crates/lash:facade_completeness",
            "//crates/lash-vm-worker:lash-vm-worker__bin",
        }
        self.assertEqual(expected, labels)
        partition = set(inventory["workspace_test_suite_labels"])
        self.assertLessEqual(set(inventory["workspace_core_suite_labels"]), partition)
        self.assertLessEqual(set(inventory["workspace_tail_suite_labels"]), partition)
        for item in warm["steps"]:
            self.assertNotRegex(item.get("run", ""), r"hermetic-build\.sh test|buck2-test\.sh")

    def test_heavy_suite_cache_keys_features_and_saves_only_main(self) -> None:
        job = self.ci["jobs"]["heavy-tests"]
        cache = step(job, "Cache heavy-suite Cargo builds")
        self.assertRegex(cache["uses"], r"^Swatinem/rust-cache@[0-9a-f]{40}$")
        settings = cache["with"]
        self.assertEqual("heavy-suites-v1", settings["shared-key"])
        self.assertEqual("${{ env.LASH_CI_FEATURES }}", settings["key"])
        self.assertEqual("${{ github.ref == 'refs/heads/main' }}", settings["save-if"])
        self.assertIs(False, settings["cache-bin"])
        self.assertIsNot(False, settings.get("cache-targets"))
        self.assertIsNot(False, settings.get("add-rust-environment-hash-key"))
        self.assertLess(job["steps"].index(step(job, "Install Rust toolchain")), job["steps"].index(cache))
        self.assertLess(job["steps"].index(cache), job["steps"].index(step(job, "Test heavy suites")))

    def test_every_remote_test_leg_keeps_reports_and_failure_artifacts(self) -> None:
        jobs = self.ci["jobs"]
        for job_name, step_name in (
            ("buck2-tests", "Test the workspace core suite with shared cache"),
            ("buck2-tests-tail", "Test the workspace tail suite with shared cache"),
            ("feature-lanes", "Run the executable feature lanes"),
            ("unicode-tests", "Test deferred Unicode suites (Buck2)"),
        ):
            with self.subTest(job=job_name):
                run = step(jobs[job_name], step_name)["run"]
                self.assertIn("scripts/ci/buck2-test.sh", run)
                upload = step(jobs[job_name], "Upload failing test logs")
                self.assertEqual("failure()", upload["if"])
                self.assertIn("failed-testlogs", upload["with"]["path"])

    def test_store_jobs_keep_the_same_suites_on_both_trust_paths(self) -> None:
        suites = []
        for job_name in (*POSTGRES_STORE_JOBS, "s3-store"):
            job = self.ci["jobs"][job_name]
            self.assertEqual("${{ needs.plan.outputs.buck2_trusted }}", job["env"]["BUCK2_TRUSTED"])
            for item in job["steps"]:
                run = item.get("run", "")
                if "scripts/ci/store-tests.sh" in run:
                    suites.append(run.split("scripts/ci/store-tests.sh", 1)[1].split()[0])
        self.assertEqual(
            [
                "pg-store",
                "pg-facade-laws",
                "pg-pool-wait",
                "pg-sim-backend-faults",
                "pg-cross-backend",
                "pg-store-synthetic-next",
                "s3-store",
                "s3-attachment-differential",
            ],
            suites,
        )
        script = (ROOT / "scripts/ci/store-tests.sh").read_text(encoding="utf-8")
        shaped = set(re.findall(r"^  ([a-z0-9-]+)\)$", script, re.MULTILINE))
        table = script.split("declare -A uniform_store_suites=(\n", 1)[1].split("\n)\n", 1)[0]
        uniform = set(re.findall(r"^\s*\[([^]]+)\]=", table, re.MULTILINE))
        self.assertEqual(set(suites), shaped | uniform)
        self.assertFalse(shaped & uniform)
        dispatcher = script.split("run_uniform_store_suite() {", 1)[1].split("\n}", 1)[0]
        self.assertIn("render_buck2_suite", dispatcher)
        self.assertIn("render_cargo_suite", dispatcher)

    def test_every_build_of_the_postgres_store_package_has_a_job(self) -> None:
        """A feature variant of a service binary cannot fall out of CI.

        The generated inventory names every PostgreSQL service binary, the
        default build and each feature variant. Each build is one suite and
        one job; together the jobs' package suites run every label once.
        """
        inventory = json.loads(
            (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
        )
        variants = {
            unit["label"] for unit in inventory["feature_lane_units"]
        } & set(inventory["service_test_targets"]["postgres"])
        self.assertTrue(variants)
        package_labels = []
        for job_name in POSTGRES_STORE_JOBS:
            job = self.ci["jobs"][job_name]
            package_suites = [
                suite for suite in store_suites(job) if suite.startswith("pg-store")
            ]
            self.assertEqual(1, len(package_suites), job_name)
            self.assertEqual(
                job_name.replace("postgres-store", "pg-store"), package_suites[0]
            )
            labels = suite_labels(package_suites[0])
            if job_name == "postgres-store":
                self.assertFalse(set(labels) & variants)
            else:
                self.assertLessEqual(set(labels), variants)
            package_labels += labels
        self.assertEqual(
            sorted(inventory["service_test_targets"]["postgres"]), sorted(package_labels)
        )

    def test_postgres_jobs_build_their_binaries_before_a_slot_opens(self) -> None:
        for job_name in POSTGRES_STORE_JOBS:
            with self.subTest(job=job_name):
                job = self.ci["jobs"][job_name]
                self.assertEqual("ubuntu-24.04", job["runs-on"])
                self.assertEqual("plan", job["needs"])
                self.assertEqual(self.ci["jobs"]["postgres-store"]["if"], job["if"])
                self.assertEqual(self.ci["jobs"]["postgres-store"]["env"], job["env"])
                names = [item.get("name") for item in job["steps"]]
                build = step(job, "Build the Postgres store test binaries")
                self.assertEqual("needs.plan.outputs.buck2_trusted == 'true'", build["if"])
                services = [
                    index for index, item in enumerate(job["steps"])
                    if "scripts/ci/with-service.sh" in item.get("run", "")
                ]
                self.assertLess(names.index(build["name"]), min(services))
                command = build["run"].replace("\\\n", " ").split()
                self.assertEqual(["bash", "scripts/ci/store-build.sh"], command[:2])
                # Every suite the job can run, whatever the event selects.
                self.assertEqual(store_suites(job), command[2:])

    def test_the_store_build_is_one_remote_build_at_the_default_jobs(self) -> None:
        suites = ["pg-store", "pg-facade-laws", "pg-pool-wait"]
        with tempfile.TemporaryDirectory() as directory:
            recorder = Path(directory) / "hermetic-build"
            calls = Path(directory) / "calls.jsonl"
            recorder.write_text(
                "#!/usr/bin/env python3\n"
                "import json, os, sys\n"
                "with open(os.environ['STORE_BUILD_CALLS'], 'a') as output:\n"
                "    output.write(json.dumps(sys.argv[1:]) + '\\n')\n",
                encoding="utf-8",
            )
            recorder.chmod(0o755)
            environment = {
                name: value for name, value in os.environ.items()
                if name != "BUCK2_TRUSTED"
            } | {
                "HERMETIC_BUILD": str(recorder),
                "RUNNER_TEMP": directory,
                "STORE_BUILD_CALLS": str(calls),
            }
            subprocess.run(
                ["bash", str(ROOT / "scripts/ci/store-build.sh"), *suites],
                cwd=ROOT, env=environment, check=True, capture_output=True, text=True,
            )
            invocation, = [
                json.loads(line) for line in calls.read_text(encoding="utf-8").splitlines()
            ]
            self.assertEqual("build", invocation[0])
            # The driver's default jobs, not the PostgreSQL slot count.
            self.assertNotIn("--jobs", invocation)
            self.assertEqual(
                "final", invocation[invocation.index("--materializations") + 1]
            )
            for option in ("--build-report", "--event-log"):
                report = Path(invocation[invocation.index(option) + 1])
                self.assertEqual(Path(directory) / "store-test-results", report.parent.parent)
            targets = [argument for argument in invocation if argument.startswith("//")]
            self.assertEqual(
                sorted({label for suite in suites for label in suite_labels(suite)}),
                targets,
            )
            self.assertEqual(targets, invocation[-len(targets):])
            unknown = subprocess.run(
                ["bash", str(ROOT / "scripts/ci/store-build.sh"), "pg-store", "conformance"],
                cwd=ROOT, env=environment, capture_output=True, text=True,
            )
            self.assertNotEqual(0, unknown.returncode)
            self.assertIn("unknown store suite: conformance", unknown.stderr)
            self.assertEqual(1, len(calls.read_text(encoding="utf-8").splitlines()))

    def test_nightly_forces_only_test_execution_uncached(self) -> None:
        nightly = workflow("test262-nightly.yml")["jobs"]["test262-full"]
        run = step(nightly, "Run the full Test262 selection uncached")["run"]
        self.assertIn("--no-test-cache", run)
        self.assertNotIn("--no-remote-cache", run)
        report = step(nightly, "Report the figures")["run"]
        self.assertIn("test262-full-test-results/root/crates/lash-typescript", report)

    def test_just_recipes_use_each_buck2_target_with_its_supported_operation(self) -> None:
        source = (ROOT / "justfile").read_text(encoding="utf-8")
        self.assertNotIn("--test_sharding_strategy", source)
        self.assertIn("kiln test //:dev_tests //:feature_lane_tests", source)
        self.assertIn("kiln clippy //:workspace_clippy", source)
        self.assertIn("kiln build //:schema_checks", source)


class TestShardHelperTests(unittest.TestCase):
    def test_shards_are_stable_disjoint_complete_and_propagate_failure(self) -> None:
        helper = ROOT / "tools/buck2/test_shard.py"
        with tempfile.TemporaryDirectory() as directory:
            temporary = Path(directory)
            calls = temporary / "calls.jsonl"
            binary = temporary / "fake-test"
            binary.write_text(
                """#!/usr/bin/env python3
import json
import os
import sys

if sys.argv[1:] == ["--list", "--format", "terse"]:
    print("alpha: test")
    print("beta: test")
    print("gamma: test")
    print("delta: test")
    print("measurement: benchmark")
    raise SystemExit(0)
with open(os.environ["SHARD_CALLS"], "a", encoding="utf-8") as output:
    output.write(json.dumps(sys.argv[1:]) + "\\n")
raise SystemExit(int(os.environ.get("SHARD_EXIT", "0")))
""",
                encoding="utf-8",
            )
            binary.chmod(0o755)
            environment = os.environ | {"SHARD_CALLS": str(calls)}
            names = {"alpha", "beta", "gamma", "delta"}
            selected: list[set[str]] = []
            rendered: list[list[str]] = []
            for index in range(3):
                result = subprocess.run(
                    [
                        "python3",
                        str(helper),
                        "3",
                        str(index),
                        str(binary),
                        "--nocapture",
                    ],
                    env=environment,
                    capture_output=True,
                    text=True,
                )
                self.assertEqual(result.returncode, 0, result.stderr)
                arguments = json.loads(
                    calls.read_text(encoding="utf-8").splitlines()[-1]
                )
                rendered.append(arguments)
                skipped = {
                    arguments[position + 1]
                    for position, argument in enumerate(arguments[:-1])
                    if argument == "--skip"
                }
                self.assertIn("--nocapture", arguments)
                self.assertNotIn("measurement", arguments)
                selected.append(names - skipped)

            self.assertEqual(set().union(*selected), names)
            self.assertEqual(sum(map(len, selected)), len(names))
            repeat = subprocess.run(
                ["python3", str(helper), "3", "1", str(binary), "--nocapture"],
                env=environment,
                capture_output=True,
                text=True,
            )
            self.assertEqual(repeat.returncode, 0, repeat.stderr)
            self.assertEqual(
                json.loads(calls.read_text(encoding="utf-8").splitlines()[-1]),
                rendered[1],
            )
            failed = subprocess.run(
                ["python3", str(helper), "3", "0", str(binary)],
                env=environment | {"SHARD_EXIT": "23"},
                capture_output=True,
                text=True,
            )
            self.assertEqual(failed.returncode, 23)

    def test_prefix_collisions_filters_ignored_and_empty_shards_are_lossless(self) -> None:
        helper = ROOT / "tools/buck2/test_shard.py"
        with tempfile.TemporaryDirectory() as directory:
            temporary = Path(directory)
            calls = temporary / "calls.jsonl"
            binary = temporary / "fake-libtest"
            binary.write_text(
                """#!/usr/bin/env python3
import json
import os
import sys

cases = [("prefix_1", False), ("prefix_1::extended", False),
         ("other", False), ("ignored_case", True)]
args = sys.argv[1:]
if args == ["--list", "--format", "terse"]:
    for name, _ignored in cases:
        print(name + ": test")
    raise SystemExit(0)
filters, skips = [], []
position = 0
while position < len(args):
    argument = args[position]
    if argument in ("--skip", "--format", "--color", "--test-threads"):
        position += 1
        if argument == "--skip":
            skips.append(args[position])
    elif not argument.startswith("-"):
        filters.append(argument)
    position += 1
exact = "--exact" in args
selected = [
    (name, ignored) for name, ignored in cases
    if (not filters or any(name == value if exact else value in name for value in filters))
    and not any(name == value if exact else value in name for value in skips)
    and ("--ignored" not in args or ignored)
]
executed = [
    name for name, ignored in selected
    if not ignored or "--ignored" in args or "--include-ignored" in args
]
with open(os.environ["SHARD_CALLS"], "a", encoding="utf-8") as output:
    output.write(json.dumps({"args": args, "executed": executed}) + "\\n")
print(f"running {len(executed)} tests")
for name in executed:
    print(f"test {name} ... ok")
print(f"test result: ok. {len(executed)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out;")
""",
                encoding="utf-8",
            )
            binary.chmod(0o755)
            environment = os.environ | {"SHARD_CALLS": str(calls)}

            def run_all(*arguments: str) -> list[list[str]]:
                calls.unlink(missing_ok=True)
                for index in range(5):
                    result = subprocess.run(
                        ["python3", str(helper), "5", str(index), str(binary), *arguments],
                        env=environment,
                        capture_output=True,
                        text=True,
                    )
                    self.assertEqual(result.returncode, 0, result.stderr)
                return [
                    json.loads(line)["executed"]
                    for line in calls.read_text(encoding="utf-8").splitlines()
                ]

            short = int.from_bytes(hashlib.sha256(b"prefix_1").digest()[:8], "big") % 5
            long = int.from_bytes(
                hashlib.sha256(b"prefix_1::extended").digest()[:8], "big"
            ) % 5
            self.assertNotEqual(short, long, "fixture must reproduce the old split")

            unfiltered = run_all()
            flattened = [name for shard in unfiltered for name in shard]
            self.assertCountEqual(flattened, ["prefix_1", "prefix_1::extended", "other"])
            self.assertEqual(len(flattened), len(set(flattened)))
            self.assertGreaterEqual(sum(not shard for shard in unfiltered), 2)
            self.assertCountEqual(
                [name for shard in run_all("prefix_1") for name in shard],
                ["prefix_1", "prefix_1::extended"],
            )
            self.assertEqual(
                [name for shard in run_all("prefix_1", "--exact") for name in shard],
                ["prefix_1"],
            )
            self.assertEqual(
                [name for shard in run_all("--skip", "prefix_1") for name in shard],
                ["other"],
            )
            self.assertEqual(
                [name for shard in run_all("--ignored") for name in shard],
                ["ignored_case"],
            )
            self.assertCountEqual(
                [name for shard in run_all("--include-ignored") for name in shard],
                ["prefix_1", "prefix_1::extended", "other", "ignored_case"],
            )


if __name__ == "__main__":
    unittest.main()
