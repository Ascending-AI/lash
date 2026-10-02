#!/usr/bin/env python3
"""Covers `scripts/ci/with-service.sh`, the one owner of service containers.

Two kinds of check live here.

The contract half holds the wrapper to the CI jobs it runs inside: every suite
the workflow dispatches goes through the wrapper, the PostgreSQL matrix majors
are all declared services, the images the wrapper names are the images CI used
to start by hand, and `store-tests.sh` keeps service execution local and fresh
while compilation uses the shared Buck2 pool.

The behaviour half runs the wrapper against a fake `docker` on PATH, so the
lifecycle it promises -- the chosen port reaching the command, teardown on a
readiness failure, teardown on Ctrl-C, `all` in declared order -- is proven
rather than asserted about the source. The `restate` service is a native
process rather than a container, so its checks run a fake `restate-server`
that answers the health and query-readiness probes the real one does.
"""

from __future__ import annotations

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
from unittest import mock

ROOT = pathlib.Path(__file__).resolve().parent.parent
WRAPPER = ROOT / "scripts" / "ci" / "with-service.sh"
STORE_TESTS = ROOT / "scripts" / "ci" / "store-tests.sh"
WORKFLOW = ROOT / ".github" / "workflows" / "ci.yml"

SERVICES = ("pg14", "pg16", "pg18", "s3", "restate")


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
                if suite == "pg-catalog-compatibility":
                    # Every compatibility major, one container each.
                    self.assertEqual("pg${major}", service)
                elif suite.startswith("pg"):
                    self.assertEqual("pg${POSTGRES_PRIMARY}", service)
                else:
                    self.assertEqual("s3", service)

    def test_every_matrix_major_is_a_declared_service(self) -> None:
        """`pg<major>` must name a service for every major the plan selects."""
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
    def test_postgres_ci_suites_get_service_and_propagate_failure(self) -> None:
        sys.path.insert(0, str(ROOT / "scripts/ci"))
        import restate_matrix

        for suite in ("server-double", "effect-group"):
            for exit_code in (0, 7):
                with self.subTest(suite=suite, exit_code=exit_code), tempfile.TemporaryDirectory() as raw:
                    directory = pathlib.Path(raw)
                    docker = FakeDocker(directory)
                    binary = directory / "fake-suite-python"
                    binary.write_text(
                        '#!/usr/bin/env bash\n'
                        'printf "%s\\n" "$*" "${LASH_POSTGRES_DATABASE_URL:-}"\n'
                        f"exit {exit_code}\n", encoding="utf-8",
                    )
                    binary.chmod(0o755)
                    env = docker.env()
                    env.pop("LASH_POSTGRES_DATABASE_URL", None)
                    results = []

                    def execute(command, **kwargs):
                        result = subprocess.run(command, **kwargs, text=True, capture_output=True, timeout=120)
                        results.append(result)
                        return result.returncode

                    with mock.patch.dict(os.environ, env, clear=True), \
                         mock.patch.object(sys, "executable", str(binary)), \
                         mock.patch.object(restate_matrix.subprocess, "call", side_effect=execute):
                        status = restate_matrix.run(suite, "replay")
                    result = results[0]
                    self.assertEqual(1 if exit_code else 0, status, result.stderr)
                    self.assertIn(f"restate_suite.py suite {suite} --leg replay --keep-test-logs", result.stdout)
                    self.assertRegex(result.stdout, r"postgres://lash:lash@127\.0\.0\.1:\d+/lash\n")
                    self.assertTrue(any(call.startswith("rm --force") for call in docker.logged()))

    def test_effect_group_recipe_requires_postgres_before_both_legs(self) -> None:
        justfile = (ROOT / "justfile").read_text(encoding="utf-8")
        recipe = justfile.split("\neffect-group-conformance-e2e:\n", 1)[1]
        recipe = recipe.split("\n# ", 1)[0]
        self.assertIn('${LASH_POSTGRES_DATABASE_URL:?', recipe)
        self.assertLess(recipe.index('${LASH_POSTGRES_DATABASE_URL:?'), recipe.index("restate_suite.py"))
        for leg in ("live", "replay"):
            self.assertIn(f"suite effect-group --leg {leg}", recipe)

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

    def test_pg16_trades_durability_for_speed_and_the_compat_lanes_do_not(self) -> None:
        """pg16's container runs without fsync/synchronous_commit/full_page_writes.

        The primary lane's database is throwaway, and its crash tests kill
        lash processes or the engine, never the host OS, so the page cache is
        all the durability it needs. The compatibility lanes run one fixed
        catalog artifact, not per-test database churn, and keep the defaults.
        """
        for name, traded in (("pg16", True), ("pg14", False), ("pg18", False)):
            with self.subTest(service=name), tempfile.TemporaryDirectory() as raw:
                result, docker = self.run_wrapper(
                    pathlib.Path(raw), [name, "--", "true"]
                )
                self.assertEqual(0, result.returncode, result.stderr)
                published = [
                    call
                    for call in docker.logged()
                    if call.startswith("run --detach")
                ]
                self.assertEqual(1, len(published))
                for flag in ("fsync", "synchronous_commit", "full_page_writes"):
                    with self.subTest(service=name, flag=flag):
                        if traded:
                            self.assertIn(f"-c {flag}=off", published[0])
                        else:
                            self.assertNotIn(f"{flag}=off", published[0])

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


# A stand-in `restate-server`: it answers health and admin query probes on the
# addresses the launcher assigns, and records its pid so a test can see it stopped.
FAKE_RESTATE_SERVER = """\
#!/usr/bin/env python3
import http.server, os, pathlib, threading

class Ok(http.server.BaseHTTPRequestHandler):
    def do_POST(self):
        if self.path != "/query":
            self.send_error(404)
            return
        self.rfile.read(int(self.headers["Content-Length"]))
        self.send_response(200)
        self.send_header("Content-Type", "application/json")
        self.end_headers()
        self.wfile.write(b'{"rows": []}')

    def do_GET(self):
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok")

    def log_message(self, *args):
        pass

pathlib.Path(os.environ["FAKE_RESTATE_PIDFILE"]).write_text(str(os.getpid()))
servers = []
for variable in ("RESTATE_ADMIN__BIND_ADDRESS", "RESTATE_INGRESS__BIND_ADDRESS"):
    host, port = os.environ[variable].rsplit(":", 1)
    servers.append(http.server.ThreadingHTTPServer((host, int(port)), Ok))
for server in servers[1:]:
    threading.Thread(target=server.serve_forever, daemon=True).start()
servers[0].serve_forever()
"""


class WithServiceRestate(unittest.TestCase):
    def run_restate(
        self, directory: pathlib.Path, command: list[str], *, docker: bool = True
    ) -> subprocess.CompletedProcess[str]:
        server = directory / "restate-server"
        server.write_text(FAKE_RESTATE_SERVER, encoding="utf-8")
        server.chmod(0o755)
        env = FakeDocker(directory).env() if docker else os.environ.copy()
        if not docker:
            # A PATH whose `docker` fails: the Restate service needs none.
            broken = directory / "docker"
            broken.write_text("#!/usr/bin/env bash\nexit 1\n", encoding="utf-8")
            broken.chmod(0o755)
            env["PATH"] = f"{directory}:{env['PATH']}"
            env.pop("GITHUB_ACTIONS", None)
        env["LASH_RESTATE_SERVER_BIN"] = str(server)
        env["FAKE_RESTATE_PIDFILE"] = str(directory / "server.pid")
        env["RESTATE_AUTHORITY_ID"] = "left-over-from-an-earlier-server"
        return subprocess.run(
            ["bash", str(WRAPPER), "restate", "--", *command],
            cwd=ROOT,
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=120,
        )

    def test_the_server_addresses_and_a_fresh_authority_reach_the_command(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            probe = (
                "import os, urllib.request\n"
                "opener = urllib.request.build_opener(urllib.request.ProxyHandler({}))\n"
                "for name, path in (('RESTATE_ADMIN_URL', '/health'),"
                " ('RESTATE_INGRESS_URL', '/restate/health')):\n"
                "    assert opener.open(os.environ[name] + path, timeout=5).status == 200\n"
                "print(os.environ['RESTATE_INGRESS_URL'])\n"
                "print(os.environ['RESTATE_ADMIN_URL'])\n"
                "print(os.environ['RESTATE_AUTHORITY_ID'])\n"
            )
            first = self.run_restate(directory, ["python3", "-c", probe])
            self.assertEqual(0, first.returncode, first.stderr)
            ingress, admin, authority = first.stdout.split()
            self.assertRegex(ingress, r"^http://127\.0\.0\.1:\d+$")
            self.assertRegex(admin, r"^http://127\.0\.0\.1:\d+$")
            self.assertNotEqual(ingress, admin)
            # The server is new, so the authority naming its state is new: a
            # value the caller carried in from an earlier server is replaced.
            self.assertNotEqual("left-over-from-an-earlier-server", authority)
            second = self.run_restate(directory, ["python3", "-c", probe])
            self.assertEqual(0, second.returncode, second.stderr)
            self.assertNotEqual(authority, second.stdout.split()[2])
            self.assertIn("restate: command passed", first.stderr)

    def test_the_server_is_stopped_when_the_command_ends(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            directory = pathlib.Path(raw)
            result = self.run_restate(directory, ["bash", "-c", "exit 7"])
            self.assertEqual(1, result.returncode)
            self.assertIn("restate: FAILED (exit 7)", result.stderr)
            pid = int((directory / "server.pid").read_text(encoding="utf-8"))
            deadline = time.monotonic() + 20
            while time.monotonic() < deadline:
                try:
                    os.kill(pid, 0)
                except ProcessLookupError:
                    break
                time.sleep(0.1)
            else:
                os.kill(pid, signal.SIGKILL)
                self.fail("the Restate server outlived the command")

    def test_the_restate_service_needs_no_container_runtime(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            result = self.run_restate(pathlib.Path(raw), ["true"], docker=False)
            self.assertEqual(0, result.returncode, result.stderr)
            self.assertNotIn("docker is unavailable", result.stderr)


if __name__ == "__main__":
    unittest.main()
