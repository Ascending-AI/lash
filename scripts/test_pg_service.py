#!/usr/bin/env python3
"""Covers `scripts/ci/pg-service.sh`, the Postgres final-server readiness probe.

The official image's entrypoint starts a temporary server for its initdb
scripts that listens on the unix socket alone (``-c listen_addresses=''``),
shuts it down, and only then starts the real server. A readiness check that
answers the container's socket — ``pg_isready`` with no host — can pass
against that temporary server and release a client into its shutdown:
``FATAL: the database system is shutting down`` is how the Restate +
Postgres workers E2E flaked when its schema apply ran mid-restart.

The contract this file holds: every readiness check the helper runs, every
compose healthcheck, and every wait the harnesses carry is TCP-bound, so
only the final server can ever answer it. The behaviour half runs
``lash_pg_wait`` against a fake exec so the deadline and the loud failure
are proven rather than asserted about the source.
"""

from __future__ import annotations

import os
import pathlib
import re
import subprocess
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
HELPER = ROOT / "scripts" / "ci" / "pg-service.sh"
PROBE_ASSIGNMENT = re.compile(r"^LASH_PG_READY_PROBE='(.*)'$", re.MULTILINE)

# Every file that starts a PostgreSQL container or checks one for readiness:
# scripts that source the helper, compose healthchecks, and GitHub service
# health commands. The sweep is by probe, not by name: a socket answer added
# anywhere fails the whole class, not just the site this change touched.
CONSUMERS = (
    "scripts/ci/with-service.sh",
    "scripts/restate-postgres-workers-e2e.sh",
    "scripts/push-gate.sh",
    "scripts/gate-container-smoke.sh",
    "scripts/confidence-gate.sh",
)
COMPOSE_FILES = (
    "runbooks/restate-postgres-workers/docker-compose.yml",
    "runbooks/process-operations/docker-compose.yml",
    "runbooks/version-bump-recreation/docker-compose.yml",
)
WORKFLOWS = (
    ".github/workflows/perf.yml",
    ".github/workflows/release.yml",
)


def probe() -> str:
    match = PROBE_ASSIGNMENT.search(HELPER.read_text(encoding="utf-8"))
    if match is None:
        raise AssertionError("pg-service.sh does not assign LASH_PG_READY_PROBE")
    return match.group(1)


def code_lines(path: pathlib.Path) -> list[str]:
    """Lines a probe can hide in: comments and blank lines dropped."""
    return [
        line
        for line in path.read_text(encoding="utf-8").splitlines()
        if line.strip() and not line.lstrip().startswith("#")
    ]


class PgServiceContract(unittest.TestCase):
    def test_the_probe_only_accepts_a_tcp_answer(self) -> None:
        text = probe()
        self.assertIn("pg_isready -h 127.0.0.1", text)
        # A ping is not a session: the query also proves auth and execution.
        self.assertIn("psql -h 127.0.0.1", text)
        self.assertEqual(text.count("pg_isready"), text.count("pg_isready -h"))
        self.assertEqual(text.count("psql"), text.count("psql -h"))

    def test_the_compose_healthchecks_carry_the_probe(self) -> None:
        """Compose config is static, so each file holds the probe literally.

        `$$` is compose's escape for a literal dollar; normalised, the
        healthcheck must be the same probe the helper exports — a healthcheck
        that drifts back to the socket re-opens the race for every service
        gated on it.
        """
        for name in COMPOSE_FILES:
            with self.subTest(compose=name):
                text = (ROOT / name).read_text(encoding="utf-8")
                self.assertIn(probe(), text.replace("$$", "$"))

    def test_the_workflow_health_commands_carry_the_probe(self) -> None:
        for name in WORKFLOWS:
            with self.subTest(workflow=name):
                text = (ROOT / name).read_text(encoding="utf-8")
                self.assertIn(probe(), text)

    def test_no_socket_probe_survives_anywhere(self) -> None:
        """Every `pg_isready` in the tree must name a TCP host.

        A bare `pg_isready` answers the container's unix socket, which the
        temporary init server also serves; `-h` is what makes the answer
        come from the final server alone.
        """
        roots = (
            list(ROOT.glob("scripts/**/*.sh"))
            + list(ROOT.glob("runbooks/**/*.yml"))
            + list(ROOT.glob(".github/workflows/*.yml"))
        )
        for path in roots:
            for line in code_lines(path):
                for occurrence in re.finditer(r"pg_isready\b", line):
                    tail = line[occurrence.end() :]
                    with self.subTest(path=path, line=line.strip()):
                        self.assertRegex(tail, r"^\s+-h\s")

    def test_every_postgres_starter_sources_the_helper(self) -> None:
        for name in CONSUMERS:
            with self.subTest(script=name):
                text = (ROOT / name).read_text(encoding="utf-8")
                self.assertRegex(
                    text, r"source.*pg-service\.sh", f"{name} does not source pg-service.sh"
                )
                self.assertIn("lash_pg_", text)

    def test_the_workers_e2e_wait_is_bounded(self) -> None:
        """The schema apply's gate: a TCP wait with a deadline, not an
        unbounded socket loop."""
        text = (ROOT / "scripts/restate-postgres-workers-e2e.sh").read_text(
            encoding="utf-8"
        )
        self.assertRegex(text, r"lash_pg_wait postgres \d+")

    def test_with_service_probes_through_the_helper(self) -> None:
        text = (ROOT / "scripts/ci/with-service.sh").read_text(encoding="utf-8")
        self.assertIn("lash_pg_ready docker exec", text)


class PgServiceBehaviour(unittest.TestCase):
    """`lash_pg_wait` against a fake exec prefix with a scripted verdict."""

    def run_wait(
        self, directory: pathlib.Path, *, seconds: int, pass_after: int
    ) -> tuple[subprocess.CompletedProcess[str], list[str]]:
        calls = directory / "calls"
        counter = directory / "counter"
        fake = directory / "fake-exec"
        fake.write_text(
            textwrap.dedent(
                """\
                #!/usr/bin/env bash
                printf '%s\\n' "$*" >>"$FAKE_CALLS"
                count="$(cat "$FAKE_COUNTER" 2>/dev/null || echo 0)"
                count=$((count + 1))
                printf '%s' "$count" >"$FAKE_COUNTER"
                [ "$count" -ge "$FAKE_PASS_AFTER" ]
                """
            ),
            encoding="utf-8",
        )
        fake.chmod(0o755)
        env = os.environ.copy()
        env["FAKE_CALLS"] = str(calls)
        env["FAKE_COUNTER"] = str(counter)
        env["FAKE_PASS_AFTER"] = str(pass_after)
        result = subprocess.run(
            [
                "bash",
                "-c",
                f'source "{HELPER}" && lash_pg_wait test-label {seconds} "{fake}"',
            ],
            env=env,
            text=True,
            capture_output=True,
            check=False,
            timeout=seconds + 30,
        )
        logged = calls.read_text(encoding="utf-8").splitlines() if calls.exists() else []
        return result, logged

    def test_a_container_that_comes_up_passes(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            result, logged = self.run_wait(
                pathlib.Path(raw), seconds=15, pass_after=3
            )
            self.assertEqual(0, result.returncode, result.stderr)
            # Failing probes were retried; the pass needed three attempts.
            self.assertGreaterEqual(len(logged), 3)
            for call in logged:
                # The prefix got the probe as one `sh -c` argument, and what
                # it ran is the TCP probe, not a socket check.
                self.assertTrue(call.startswith("sh -c "), call)
                self.assertIn("-h 127.0.0.1", call)

    def test_a_container_that_never_comes_up_fails_loudly(self) -> None:
        with tempfile.TemporaryDirectory() as raw:
            result, logged = self.run_wait(
                pathlib.Path(raw), seconds=3, pass_after=99
            )
            self.assertEqual(1, result.returncode)
            self.assertIn("test-label", result.stderr)
            self.assertIn("TCP", result.stderr)
            self.assertGreaterEqual(len(logged), 2)


if __name__ == "__main__":
    unittest.main()
