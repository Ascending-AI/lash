#!/usr/bin/env python3
"""Covers `scripts/ci/pg-service.sh`, the Postgres final-server readiness probe.

The official image's entrypoint starts a temporary server for its initdb
scripts that listens on the unix socket alone (``-c listen_addresses=''``),
shuts it down, and only then starts the real server. A readiness check that
answers the container's socket — ``pg_isready`` with no host — can pass
against that temporary server and release a client into its shutdown:
``FATAL: the database system is shutting down`` is how a PostgreSQL-backed
E2E flaked when its schema apply ran mid-restart.

The contract this file holds: every readiness check the helper runs and every
wait the harnesses carry is TCP-bound, so
only the final server can ever answer it. The behaviour half runs
``lash_pg_wait`` against a fake exec so the deadline and the loud failure
are proven rather than asserted about the source.
"""

from __future__ import annotations

import os
import pathlib
import subprocess
import tempfile
import textwrap
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent
HELPER = ROOT / "scripts" / "ci" / "pg-service.sh"


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
