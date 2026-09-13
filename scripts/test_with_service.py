#!/usr/bin/env python3
"""Covers `scripts/ci/with-service.sh`, the one owner of service containers.

Two kinds of check live here.

The contract half holds the wrapper to the CI jobs it runs inside: every suite
the workflow dispatches goes through the wrapper, the PostgreSQL matrix majors
are all declared services, the images the wrapper names are the images CI used
to start by hand, and `store-tests.sh` still keeps its shared-cache environment
required inside CI while supplying the `kiln build` configuration outside it.

The behaviour half runs the wrapper against a fake `docker` on PATH, so the
lifecycle it promises -- the chosen port reaching the command, teardown on a
readiness failure, teardown on Ctrl-C, `all` in declared order -- is proven
rather than asserted about the source.
"""

from __future__ import annotations

import os
import pathlib
import re
import signal
import subprocess
import tempfile
import textwrap
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
WRAPPER = ROOT / "scripts" / "ci" / "with-service.sh"
STORE_TESTS = ROOT / "scripts" / "ci" / "store-tests.sh"
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"

SERVICES = ("pg14", "pg16", "pg18", "s3")


def workflow_text() -> str:
    return WORKFLOW.read_text(encoding="utf-8")


def wrapper_text() -> str:
    return WRAPPER.read_text(encoding="utf-8")


def wrapped_suites() -> list[tuple[str, str]]:
    """`(service expression, suite)` for every wrapped store-tests.sh call."""
    return re.findall(
        r"bash scripts/ci/with-service\.sh \"?([^\"\s]+)\"? -- \\\n"
        r"\s*bash scripts/ci/store-tests\.sh ([a-z0-9-]+)\s*$",
        workflow_text(),
        flags=re.MULTILINE,
    )


class WithServiceContract(unittest.TestCase):
    def test_every_ci_store_suite_goes_through_the_wrapper(self) -> None:
        wrapped = wrapped_suites()
        self.assertTrue(wrapped, "the workflow dispatches no wrapped store suites")
        # A bare call would stand up no service at all, or worse, quietly reuse
        # one another step left running.
        bare = re.findall(
            r"^\s*run: bash scripts/ci/store-tests\.sh",
            workflow_text(),
            flags=re.MULTILINE,
        )
        self.assertEqual([], bare, "a store suite runs outside with-service.sh")
        for service, suite in wrapped:
            with self.subTest(suite=suite):
                if suite.startswith("pg"):
                    self.assertEqual("pg${{ matrix.postgres }}", service)
                else:
                    self.assertEqual("s3", service)

    def test_every_matrix_major_is_a_declared_service(self) -> None:
        """`pg${{ matrix.postgres }}` must name a service for every major."""
        plan = (ROOT / "scripts" / "ci_plan.py").read_text(encoding="utf-8")
        majors = set(re.findall(r'"postgres":\s*"(\d+)"', plan))
        self.assertTrue(majors)
        table = wrapper_text()
        for major in sorted(majors):
            with self.subTest(major=major):
                self.assertIn(f"pg{major})", table)
                self.assertIn(f"postgres:{major}-alpine", table)

    def test_images_are_declared_once_and_only_in_the_wrapper(self) -> None:
        """CI starts nothing itself, so it names no image."""
        workflow = workflow_text()
        for image in ("postgres:", "quay.io/minio/"):
            self.assertNotIn(image, workflow, f"ci.yml still names {image}")
        table = wrapper_text()
        self.assertIn("quay.io/minio/minio:RELEASE.2025-04-22T22-12-26Z", table)
        self.assertIn("quay.io/minio/mc:RELEASE.2025-04-16T18-13-26Z", table)
        # The settings CI's `services:` block used to pass.
        self.assertIn("shared_preload_libraries=pg_stat_statements", table)
        self.assertIn("POSTGRES_USER=lash", table)

    def test_no_host_port_is_hardcoded(self) -> None:
        """The wrapper picks a free port; a literal would collide between lanes."""
        table = wrapper_text()
        for name, value in re.findall(r"\"(LASH_[A-Z_]+)=([^\"]*)\"", table):
            with self.subTest(name=name):
                if "127.0.0.1" in value:
                    self.assertIn("${port}", value)
        self.assertNotIn("127.0.0.1:5432", table)
        self.assertNotIn("127.0.0.1:9000", table)

    def test_wrapper_supplies_the_names_store_tests_forwards(self) -> None:
        """Every variable the wrapper exports must reach the test spawn."""
        script = STORE_TESTS.read_text(encoding="utf-8")
        forwarded = set(re.findall(r"--test_env=([A-Z_]+)", script))
        exported = set(re.findall(r"\"(LASH_[A-Z_]+)=", wrapper_text()))
        self.assertTrue(exported)
        self.assertLessEqual(exported, forwarded)

    def test_store_tests_keeps_ci_strict_and_supplies_the_kiln_configuration(
        self,
    ) -> None:
        """The generalisation must not weaken CI.

        Inside GitHub Actions both shared-cache variables stay required: an
        unset value there means the credentials step did not run, and a build
        that quietly missed the shared cache is the failure this refuses. The
        default outside CI is the same `--config=shared` `kiln build` uses.
        """
        script = STORE_TESTS.read_text(encoding="utf-8")
        self.assertIn('if [ -n "${GITHUB_ACTIONS:-}" ]; then', script)
        self.assertIn('"${BAZEL_SHARED_CACHE_FLAGS:?', script)
        self.assertIn('"${BAZEL_OUTPUT_USER_ROOT:?', script)
        self.assertIn(
            ': "${BAZEL_SHARED_CACHE_FLAGS=--config=shared'
            ' --strategy=TestRunner=local}"',
            script,
        )
        # The trust decision itself is never defaulted by store-tests.sh: a run
        # that cannot say which path it is on must fail, not guess. The wrapper
        # takes the trusted, pool-built path for a local run, and never
        # overrides the decision CI already made.
        self.assertIn('trusted="${BAZEL_TRUSTED:?BAZEL_TRUSTED must be', script)
        self.assertIn('export BAZEL_TRUSTED="${BAZEL_TRUSTED:-true}"', wrapper_text())

    def test_not_covered_names_runnable_recipes(self) -> None:
        """The closing report must not send a reader to a recipe that is gone."""
        listing = subprocess.run(
            ["bash", str(WRAPPER), "--list"],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=True,
        ).stdout
        self.assertIn("NOT covered", listing)
        for name in SERVICES:
            self.assertIn(name, listing)
        recipes = re.findall(r"^\s*run: (.+)$", listing, flags=re.MULTILINE)
        self.assertTrue(recipes)
        justfile = (ROOT / "justfile").read_text(encoding="utf-8")
        for recipe in recipes:
            with self.subTest(recipe=recipe):
                if recipe.startswith("just "):
                    target = recipe.split()[1]
                    self.assertRegex(justfile, rf"(?m)^{re.escape(target)}[ :]")
                elif recipe.startswith("bash "):
                    self.assertTrue((ROOT / recipe.split()[1]).is_file())
                else:
                    self.assertTrue(recipe.startswith("cargo "))


class FakeDocker:
    """A `docker` on PATH that logs its calls and answers scripted probes."""

    def __init__(self, directory: pathlib.Path, *, ready: bool = True) -> None:
        self.directory = directory
        self.calls = directory / "docker-calls"
        script = directory / "docker"
        # `exec` fails (exit 1) until the marker exists when ready is False,
        # which is how a service that never comes up presents itself.
        script.write_text(
            textwrap.dedent(
                f"""\
                #!/usr/bin/env bash
                printf '%s\\n' "$*" >>'{self.calls}'
                case "$1" in
                  exec) exit {0 if ready else 1} ;;
                  run)
                    for arg in "$@"; do
                      if [ "$arg" = alias ]; then exit {0 if ready else 1}; fi
                    done
                    exit 0
                    ;;
                  *) exit 0 ;;
                esac
                """
            ),
            encoding="utf-8",
        )
        script.chmod(0o755)

    def logged(self) -> list[str]:
        if not self.calls.exists():
            return []
        return self.calls.read_text(encoding="utf-8").splitlines()

    def env(self) -> dict[str, str]:
        merged = os.environ.copy()
        merged["PATH"] = f"{self.directory}:{merged['PATH']}"
        # The wrapper must not reach the shared cache or a real bazel here.
        merged["BAZEL_TRUSTED"] = "false"
        merged.pop("GITHUB_ACTIONS", None)
        return merged


class WithServiceBehaviour(unittest.TestCase):
    def run_wrapper(
        self,
        directory: pathlib.Path,
        args: list[str],
        *,
        ready: bool = True,
        timeout: int = 120,
    ) -> tuple[subprocess.CompletedProcess[str], FakeDocker]:
        docker = FakeDocker(directory, ready=ready)
        result = subprocess.run(
            ["bash", str(WRAPPER), *args],
            cwd=ROOT,
            env=docker.env(),
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout,
        )
        return result, docker

    def test_the_chosen_port_reaches_the_command(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            result, docker = self.run_wrapper(
                directory,
                ["pg16", "--", "bash", "-c", 'echo "$LASH_POSTGRES_DATABASE_URL"'],
            )
            self.assertEqual(0, result.returncode, result.stderr)
            url = result.stdout.strip()
            # The wrapped command owns stdout; the wrapper's own notes and its
            # closing coverage report go to stderr.
            self.assertNotIn("NOT covered", result.stdout)
            port = int(url.rsplit(":", 1)[1].split("/")[0])
            # An ephemeral port, never the container's own 5432.
            self.assertNotEqual(5432, port)
            self.assertGreater(port, 1024)
            self.assertEqual(f"postgres://lash:lash@127.0.0.1:{port}/lash", url)
            published = [
                call for call in docker.logged() if call.startswith("run --detach")
            ]
            self.assertEqual(1, len(published))
            self.assertIn(f"--publish 127.0.0.1:{port}:5432", published[0])

    def test_the_container_is_removed_after_a_passing_command(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            result, docker = self.run_wrapper(directory, ["pg16", "--", "true"])
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertTrue(
                any(call.startswith("rm --force") for call in docker.logged()),
                docker.logged(),
            )

    def test_a_readiness_failure_tears_the_container_down(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            marker = pathlib.Path(raw) / "ran"
            result, docker = self.run_wrapper(
                directory,
                ["pg16", "--", "bash", "-c", f"touch '{marker}'"],
                ready=False,
                # 30 probes two seconds apart, the CI health-check budget.
                timeout=240,
            )
            self.assertEqual(1, result.returncode)
            # The command must never run against a service that never came up.
            self.assertFalse(marker.exists())
            self.assertIn("never became ready", result.stderr)
            self.assertTrue(
                any(call.startswith("rm --force") for call in docker.logged()),
                docker.logged(),
            )
            # The container's own log is what a reader needs to diagnose it.
            self.assertTrue(
                any(call.startswith("logs --tail") for call in docker.logged())
            )

    def test_an_interrupt_tears_the_container_down(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            docker = FakeDocker(directory)
            started = directory / "started"
            process = subprocess.Popen(
                [
                    "bash",
                    str(WRAPPER),
                    "pg16",
                    "--",
                    "bash",
                    "-c",
                    f"touch '{started}'; sleep 60",
                ],
                cwd=ROOT,
                env=docker.env(),
                text=True,
                stdout=subprocess.PIPE,
                stderr=subprocess.PIPE,
                # Ctrl-C reaches the whole foreground process group, which is
                # what makes bash run the trap instead of waiting out the
                # command it is blocked on.
                start_new_session=True,
            )
            deadline = time.monotonic() + 60
            while not started.exists() and time.monotonic() < deadline:
                time.sleep(0.1)
            self.assertTrue(started.exists(), "the command never started")
            os.killpg(process.pid, signal.SIGINT)
            _, stderr = process.communicate(timeout=60)
            self.assertIn("interrupted; containers removed", stderr)
            self.assertTrue(
                any(call.startswith("rm --force") for call in docker.logged()),
                docker.logged(),
            )

    def test_all_runs_every_service_in_declared_order(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            log = directory / "order"
            result, _ = self.run_wrapper(
                directory,
                [
                    "all",
                    "--",
                    "bash",
                    "-c",
                    f"printf '%s\\n' \"${{LASH_POSTGRES_DATABASE_URL:-s3}}\" >>'{log}'",
                ],
            )
            self.assertEqual(0, result.returncode, result.stderr)
            seen = log.read_text(encoding="utf-8").splitlines()
            self.assertEqual(4, len(seen), seen)
            self.assertEqual("s3", seen[-1])
            self.assertIn("passed: pg14 pg16 pg18 s3", result.stderr)

    def test_a_failing_command_fails_the_wrapper(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            result, _ = self.run_wrapper(
                pathlib.Path(raw), ["s3", "--", "bash", "-c", "exit 7"]
            )
            self.assertEqual(1, result.returncode)
            self.assertIn("FAILED (exit 7)", result.stderr)

    def test_an_unknown_service_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            result, docker = self.run_wrapper(
                pathlib.Path(raw), ["pg15", "--", "true"]
            )
            self.assertEqual(2, result.returncode)
            self.assertIn("unknown service 'pg15'", result.stderr)
            self.assertEqual([], docker.logged())

    def test_a_service_without_a_command_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            result, _ = self.run_wrapper(pathlib.Path(raw), ["pg16"])
            self.assertEqual(2, result.returncode)
            self.assertIn("no command given", result.stderr)


if __name__ == "__main__":
    unittest.main()
