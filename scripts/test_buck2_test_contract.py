#!/usr/bin/env python3
"""Contract tests for trusted Buck2 CI and untrusted Cargo fallback paths."""

from __future__ import annotations

import json
import os
from pathlib import Path
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

    def test_installs_only_the_repository_pinned_official_client(self) -> None:
        self.assertIn("python3 tools/buck2/bootstrap.py", self.text)
        self.assertNotIn("client-Cargo.lock", self.text)
        self.assertNotIn("patches/**", self.text)
        self.assertNotIn("bazel", self.text.lower())

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

    def test_all_credential_lines_are_masked_before_outputs(self) -> None:
        verify = step(shared_action(), "Verify shared pool configuration")["run"]
        credentials = {
            "CACHE_CA": "sentinel-ca-first\nsentinel-ca-second\nsentinel-ca-last",
            "CACHE_CERT": "sentinel-cert-first\nsentinel-cert-second\nsentinel-cert-last",
            "CACHE_KEY": "sentinel-key-first\nsentinel-key-second\nsentinel-key-last",
        }
        with tempfile.TemporaryDirectory(dir=ROOT / ".buck2") as directory:
            output = Path(directory) / "output"
            # A FIFO records outputs in the same stream as masks, preserving order.
            os.mkfifo(output)
            completed = subprocess.run(
                ["bash", "-c", 'cat "$GITHUB_OUTPUT" & reader=$!\n' + verify + '\nwait "$reader"'],
                cwd=ROOT,
                env=os.environ | credentials | {
                    "CACHE_ENDPOINT": "grpcs://sentinel.example:8443",
                    "CACHE_INSTANCE": "sentinel-instance",
                    "KILN_EXECUTOR_RUNTIME": "sentinel-runtime",
                    "GITHUB_OUTPUT": str(output),
                }, capture_output=True, text=True,
            )
        self.assertEqual(completed.returncode, 0, completed.stderr)
        lines = completed.stdout.splitlines()
        first_output = lines.index("address=sentinel.example:8443")
        for value in credentials.values():
            for line in value.splitlines():
                with self.subTest(credential=line):
                    mask = "::add-mask::" + line
                    self.assertEqual(lines.count(mask), 1)
                    self.assertLess(lines.index(mask), first_output)

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

    def test_untrusted_workspace_and_lint_paths_stay_cargo(self) -> None:
        workspace = self.ci["jobs"]["workspace-tests"]
        self.assertIn("buck2_trusted != 'true'", str(workspace["if"]))
        self.assertIn("cargo nextest", str(workspace))
        lint = self.ci["jobs"]["lint"]
        cargo = step(lint, "Clippy (workspace, all targets)")
        self.assertIn("buck2_trusted != 'true'", str(cargo["if"]))
        self.assertIn("cargo clippy --workspace --all-targets", cargo["run"])

    def test_push_static_checks_are_a_separate_visible_job(self) -> None:
        parsed = yaml.load(
            (ROOT / ".github/workflows/static-checks.yml").read_text(),
            Loader=yaml.BaseLoader,
        )
        self.assertEqual({"push"}, set(parsed["on"]))
        self.assertEqual(["main"], parsed["on"]["push"]["branches"])
        self.assertEqual({"static-checks"}, set(parsed["jobs"]))
        self.assertEqual("ci-static-checks-main", parsed["concurrency"]["group"])
        self.assertEqual("false", parsed["concurrency"]["cancel-in-progress"])
        static = parsed["jobs"]["static-checks"]
        self.assertNotIn("needs", static)
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


if __name__ == "__main__":
    unittest.main()
