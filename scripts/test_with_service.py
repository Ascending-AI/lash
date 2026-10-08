#!/usr/bin/env python3
"""Covers `scripts/ci/with-service.sh`, the one owner of service containers.

Two kinds of check live here.

The contract half holds the wrapper to the CI jobs it runs inside: every suite
the workflow dispatches goes through the wrapper, PostgreSQL 18 is CI's one
major and 17 runs in the release gate alone, the images the wrapper names are the images CI used
to start by hand, and `store-tests.sh` keeps service execution local and fresh
while compilation uses the shared Buck2 pool.

The behaviour half runs the wrapper against a fake `docker` on PATH, so the
lifecycle it promises -- the chosen port reaching the command, teardown on a
readiness failure, teardown on Ctrl-C, `all` in declared order -- is proven
rather than asserted about the source.
"""

from __future__ import annotations

import json
import os
import pathlib
import re
import signal
import subprocess
import sys
import tempfile
import textwrap
import time
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
WRAPPER = ROOT / "scripts" / "ci" / "with-service.sh"
STORE_TESTS = ROOT / "scripts" / "ci" / "store-tests.sh"
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"
RELEASE = ROOT / ".github" / "workflows" / "release.yml"
REHEARSAL = ROOT / "scripts" / "release-rehearsal.sh"

SERVICES = ("pg", "pg17", "s3")
PINNED_POSTGRES = re.compile(r"postgres:(\d+)-alpine@(sha256:[0-9a-f]{64})")


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
                    self.assertEqual("pg", service)
                else:
                    self.assertEqual("s3", service)

    def test_postgresql_18_and_17_are_the_postgres_services(self) -> None:
        """Lash 1.0 supports 17 and 18: `pg` runs 18 and `pg17` runs 17.

        Each image is pinned by index digest, and no other PostgreSQL image
        is named.
        """
        table = wrapper_text()
        self.assertEqual(
            ["18", "17"], [major for major, _ in PINNED_POSTGRES.findall(table)]
        )
        self.assertEqual(2, len(re.findall(r"postgres:[0-9a-z.-]+", table)))
        listing = subprocess.run(
            ["bash", str(WRAPPER), "--list"],
            cwd=ROOT,
            text=True,
            capture_output=True,
            check=True,
        ).stdout
        images = dict(line.split("\t")[:2] for line in listing.splitlines() if "\t" in line)
        self.assertRegex(images["pg"], r"^postgres:18-alpine@sha256:")
        self.assertRegex(images["pg17"], r"^postgres:17-alpine@sha256:")

    def test_the_release_pins_the_wrappers_18(self) -> None:
        """release.yml's own PostgreSQL service is the 18 the wrapper runs."""
        pinned = dict(PINNED_POSTGRES.findall(wrapper_text()))
        release = RELEASE.read_text(encoding="utf-8")
        self.assertEqual(
            [f"postgres@{pinned['18']} # postgres:18-alpine"],
            re.findall(r"postgres@sha256:[0-9a-f]{64} # postgres:[0-9a-z-]+", release),
        )

    def test_17_runs_in_the_release_gate_alone(self) -> None:
        """CI never starts 17; the release runs every PostgreSQL leg on both.

        The release workflow's `release-postgres` job runs `pg-release` under
        `pg` and `pg17`, and publishing waits for it; the cut rehearsal runs
        the same suite on both majors.
        """
        self.assertNotIn("pg17", workflow_text())
        import yaml

        release = yaml.safe_load(RELEASE.read_text(encoding="utf-8"))
        job = release["jobs"]["release-postgres"]
        self.assertEqual(
            ["pg", "pg17"], [row["service"] for row in job["strategy"]["matrix"]["include"]]
        )
        runs = "\n".join(step.get("run", "") for step in job["steps"])
        self.assertIn("scripts/ci/store-build.sh pg-release", runs)
        self.assertIn('with-service.sh "${SERVICE}" -- \\\n', runs)
        self.assertIn("bash scripts/ci/store-tests.sh pg-release", runs)
        self.assertIn("release-postgres", release["jobs"]["publish-crates"]["needs"])
        self.assertIn(
            "bash scripts/ci/with-service.sh pg pg17 -- bash scripts/ci/store-tests.sh pg-release",
            REHEARSAL.read_text(encoding="utf-8"),
        )

    def test_images_are_declared_once_and_only_in_the_wrapper(self) -> None:
        """CI starts nothing itself, so it names no image."""
        workflow = workflow_text()
        for image in ("postgres:", "dxflrs/garage"):
            self.assertNotIn(image, workflow, f"ci.yml still names {image}")
        table = wrapper_text()
        # The S3 service's image is Garage, pinned by digest in the file the
        # runbooks and gates share, and the wrapper takes it from there.
        s3_service = (ROOT / "scripts" / "ci" / "s3-service.sh").read_text(encoding="utf-8")
        self.assertRegex(s3_service, r'LASH_S3_IMAGE="dxflrs/garage:v[0-9.]+@sha256:[0-9a-f]{64}"')
        self.assertIn('source "$(dirname "${BASH_SOURCE[0]}")/s3-service.sh"', table)
        self.assertIn('echo "$LASH_S3_IMAGE"', table)
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
        exported = set(re.findall(r"\"(LASH_[A-Z_]+)=", wrapper_text()))
        self.assertTrue(exported)
        self.assertIn('test_env+=(--test_env "$name")', script)
        for name in exported:
            with self.subTest(name=name):
                self.assertIn(name, script)

    def test_store_tests_keeps_compilation_remote_and_service_execution_local(self) -> None:
        script = STORE_TESTS.read_text(encoding="utf-8")
        self.assertIn('"${HERMETIC_BUILD:-scripts/hermetic-build.sh}" test', script)
        self.assertIn("--local-test-execution", script)
        self.assertIn("--no-test-cache", script)
        self.assertIn('--jobs "${LASH_POSTGRES_SLOT_COUNT:-32}"', script)
        self.assertIn("--test_env=LASH_POSTGRES_SLOT_DIR", script)
        self.assertNotIn("--no-remote-cache", script)
        # The trust decision itself is never defaulted by store-tests.sh: a run
        # that cannot say which path it is on must fail, not guess. The wrapper
        # takes the trusted, pool-built path for a local run, and never
        # overrides the decision CI already made.
        self.assertIn('trusted="${BUCK2_TRUSTED:?BUCK2_TRUSTED must be', script)
        self.assertIn('export BUCK2_TRUSTED="${BUCK2_TRUSTED:-true}"', wrapper_text())

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
                elif recipe.startswith(("bash ", "python3 ")):
                    self.assertTrue((ROOT / recipe.split()[1]).is_file())
                else:
                    self.assertTrue(recipe.startswith("cargo "))


class PostgresSlotBudgetTests(unittest.TestCase):
    """FIG-5340: slot admission must bound libtest's connection multiplier."""

    def test_shared_server_admits_one_test_thread_per_slot(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            runner = directory / "postgres_slot_runner.sh"
            runner.write_text(
                (ROOT / "tools/buck2/postgres_slot_runner.sh").read_text(),
                encoding="utf-8",
            )
            # The final executor reports the admitted process's environment.
            # It stays alive so four owners compete with a fifth action.
            (directory / "test_xml_runner.sh").write_text(
                'exec "$@"\n', encoding="utf-8"
            )
            probe = directory / "probe.py"
            probe.write_text(
                "import json, os, pathlib, time\n"
                "work = pathlib.Path(os.environ['PROBE_DIR'])\n"
                "(work / (os.environ['PROBE_ID'] + '.json')).write_text(json.dumps({\n"
                " 'threads': int(os.environ.get('RUST_TEST_THREADS', '32')),\n"
                " 'matrix': int(os.environ.get('LASH_MATRIX_THREADS', '32')),\n"
                " 'url': os.environ['LASH_POSTGRES_DATABASE_URL']}))\n"
                "while not (work / 'release').exists(): time.sleep(.01)\n",
                encoding="utf-8",
            )
            environment = os.environ | {
                "LASH_POSTGRES_SLOT_DIR": raw,
                "LASH_POSTGRES_SLOT_COUNT": "4",
                "LASH_POSTGRES_DATABASE_URL": "postgres://localhost/lash?application_name=law",
                "RUST_TEST_THREADS": "32",
                "LASH_MATRIX_THREADS": "32",
                "PROBE_DIR": raw,
            }
            children = []
            try:
                for index in range(5):
                    children.append(subprocess.Popen(
                        ["bash", str(runner), sys.executable, str(probe)],
                        env=environment | {"PROBE_ID": str(index)},
                        stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True,
                    ))
                deadline = time.monotonic() + 10
                while len(list(directory.glob("*.json"))) < 4 and time.monotonic() < deadline:
                    time.sleep(.01)
                owners = [json.loads(path.read_text()) for path in directory.glob("*.json")]
                self.assertEqual(4, len(owners), "one action must own each of four slots")
                self.assertEqual(4, len({owner["url"] for owner in owners}))
                self.assertTrue(all(owner["url"].endswith("?application_name=law") for owner in owners))
                # Four slots with the old 32 threads and seven measured
                # connections per cell demand 896 sessions from a 400-session
                # server. Serial libtest leaves capacity for reopened stores,
                # node/listener connections and fixture maintenance.
                self.assertEqual(4, sum(owner["threads"] for owner in owners),
                                 "shared-server slots multiply unbounded libtest concurrency")
                self.assertEqual(16, sum(owner["threads"] * owner["matrix"] for owner in owners),
                                 "serial libtest must also bound the cells inside a matrix law")
            finally:
                (directory / "release").touch()
                for child in children:
                    try:
                        stdout, stderr = child.communicate(timeout=10)
                    except subprocess.TimeoutExpired:
                        child.kill()
                        child.communicate()
                        raise
                    self.assertEqual(0, child.returncode, stdout + stderr)

    def test_explicit_threads_cannot_overdraw_the_shared_server(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            runner = directory / "postgres_slot_runner.sh"
            runner.write_text(
                (ROOT / "tools/buck2/postgres_slot_runner.sh").read_text(),
                encoding="utf-8",
            )
            (directory / "test_xml_runner.sh").write_text('exec "$@"\n', encoding="utf-8")
            for arguments in (("--test-threads=8",), ("--test-threads", "8")):
                with self.subTest(arguments=arguments):
                    result = subprocess.run(
                        ["bash", str(runner), "/bin/true", *arguments],
                        env=os.environ | {
                            "LASH_POSTGRES_SLOT_DIR": raw,
                            "LASH_POSTGRES_SLOT_COUNT": "4",
                            "LASH_POSTGRES_DATABASE_URL": "postgres://localhost/lash",
                            "XML_OUTPUT_FILE": str(pathlib.Path(raw) / "result.xml"),
                        },
                        capture_output=True, text=True, timeout=10,
                    )
                    self.assertEqual(2, result.returncode, result.stdout + result.stderr)
                    self.assertIn("one libtest thread per PostgreSQL slot", result.stderr)


def terminal_signals() -> None:
    """Give the spawned wrapper the signal state a terminal login has.

    `preexec_fn` for the wrapper spawn below. `exec` preserves a signal
    disposition of SIG_IGN, and callers can legitimately start this suite
    with SIGINT ignored -- a backgrounded command in a non-interactive shell
    is the usual cause, and it is how the gate runner can be reached. A bash
    that enters with SIGINT ignored cannot trap it (`trap ... INT` becomes a
    silent no-op), so the interrupt this test sends would vanish: the wrapper
    would wait out the whole `sleep 60` and never print its trap's line. The
    wrapped command and its children inherit the reset too.
    """
    for sig in (signal.SIGINT, signal.SIGQUIT):
        signal.signal(sig, signal.SIG_DFL)
    signal.pthread_sigmask(signal.SIG_UNBLOCK, {signal.SIGINT, signal.SIGQUIT})


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
        if not ready:
            sleep = directory / "sleep"
            sleep.write_text(
                "#!/usr/bin/env bash\n"
                f"printf 'sleep %s\\n' \"$*\" >>'{self.calls}'\n",
                encoding="utf-8",
            )
            sleep.chmod(0o755)

    def logged(self) -> list[str]:
        if not self.calls.exists():
            return []
        return self.calls.read_text(encoding="utf-8").splitlines()

    def env(self) -> dict[str, str]:
        merged = os.environ.copy()
        merged["PATH"] = f"{self.directory}:{merged['PATH']}"
        # The wrapper must not reach the shared cache or a real Buck2 client here.
        merged["BUCK2_TRUSTED"] = "false"
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
        env = docker.env()
        env["XDG_CACHE_HOME"] = str(self.cached_postgres_17_tree(directory))
        result = subprocess.run(
            ["bash", str(WRAPPER), *args],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=timeout,
        )
        return result, docker

    @staticmethod
    def cached_postgres_17_tree(directory: pathlib.Path) -> pathlib.Path:
        """A cache holding a current PostgreSQL 17 tree, so `pg17` fetches nothing."""
        text = wrapper_text()
        version = re.search(r'PG17_TREE_VERSION="([0-9.]+)"', text).group(1)
        digest = re.search(r'PG17_TREE_SHA256="([0-9a-f]{64})"', text).group(1)
        cache = directory / "cache"
        tree = cache / "lash" / f"postgres-{version}"
        tree.mkdir(parents=True, exist_ok=True)
        (tree / ".lash-postgres-tree").write_text(digest, encoding="utf-8")
        return cache

    def test_pg17_hands_its_server_tree_to_the_command(self) -> None:
        """A leg that owns its server runs the major its service names.

        `pg17` exports the pinned 17 tree as LASH_WORKERS_POSTGRES; `pg`
        exports nothing, so such a leg keeps its target's 18.
        """
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            command = ["--", "bash", "-c", 'echo "${LASH_WORKERS_POSTGRES:-unset}"']
            result, _ = self.run_wrapper(directory, ["pg17", *command])
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertRegex(
                result.stdout.strip(), rf"^{re.escape(raw)}/cache/lash/postgres-17\.[0-9.]+$"
            )
            result, _ = self.run_wrapper(directory, ["pg", *command])
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertEqual("unset", result.stdout.strip())

    def test_the_chosen_port_reaches_the_command(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            result, docker = self.run_wrapper(
                directory,
                ["pg", "--", "bash", "-c", 'echo "$LASH_POSTGRES_DATABASE_URL"'],
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

    def test_pg_trades_durability_for_speed(self) -> None:
        """Both PostgreSQL containers run without fsync/synchronous_commit/full_page_writes.

        Their databases are throwaway, and their crash tests kill lash
        processes or the engine, never the host OS, so the page cache is all
        the durability they need.
        """
        for service in ("pg", "pg17"):
            with self.subTest(service=service), tempfile.TemporaryDirectory() as raw:
                result, docker = self.run_wrapper(pathlib.Path(raw), [service, "--", "true"])
                self.assertEqual(0, result.returncode, result.stderr)
                published = [
                    call for call in docker.logged() if call.startswith("run --detach")
                ]
                self.assertEqual(1, len(published))
                for flag in ("fsync", "synchronous_commit", "full_page_writes"):
                    with self.subTest(service=service, flag=flag):
                        self.assertIn(f"-c {flag}=off", published[0])

    def test_the_container_is_removed_after_a_passing_command(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            result, docker = self.run_wrapper(directory, ["pg", "--", "true"])
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
                ["pg", "--", "bash", "-c", f"touch '{marker}'"],
                ready=False,
            )
            self.assertEqual(1, result.returncode)
            # The command must never run against a service that never came up.
            self.assertFalse(marker.exists())
            self.assertIn("never became ready", result.stderr)
            probes = [call for call in docker.logged() if call.startswith("exec ")]
            self.assertEqual(30, len(probes))
            self.assertEqual(30, docker.logged().count("sleep 2"))
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
                    "pg",
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
                preexec_fn=terminal_signals,
            )
            # `started` exists only once the wrapped command runs, which is
            # after the wrapper installed its traps; signalling earlier could
            # land before the INT trap exists.
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
            self.assertEqual(2, len(seen), seen)
            self.assertEqual("s3", seen[-1])
            self.assertIn("passed: pg s3", result.stderr)

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
                pathlib.Path(raw), ["mysql", "--", "true"]
            )
            self.assertEqual(2, result.returncode)
            self.assertIn("unknown service 'mysql'", result.stderr)
            self.assertEqual([], docker.logged())

    def test_a_service_without_a_command_is_refused(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            result, _ = self.run_wrapper(pathlib.Path(raw), ["pg"])
            self.assertEqual(2, result.returncode)
            self.assertIn("no command given", result.stderr)


if __name__ == "__main__":
    unittest.main()
