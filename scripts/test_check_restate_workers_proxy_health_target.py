#!/usr/bin/env python3
"""Tests for scripts/check_restate_workers_proxy_health_target.py.

Every test asserts the check *fails* on a specific way of putting the liveness
probe back on the invocation connection, because a check that has only ever been
seen to pass proves nothing. The repository's own harness is exercised last, so
the rule is known to be narrow enough for the shape the harness actually uses.
"""

from __future__ import annotations

from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parent))

import check_restate_workers_proxy_health_target as checker  # noqa: E402


FIXED = """{
\tservers {
\t\tprotocols h1 h2c
\t}
}

:18100 {
\treverse_proxy h2c://worker-a:18100 h2c://worker-b:18100 {
\t\tlb_policy round_robin
\t\thealth_port 18101
\t\thealth_uri /health
\t\thealth_interval 1s
\t\thealth_timeout 1s
\t}
}
"""

WORKER_SOURCE = """
    let control_port = env("WORKER_CONTROL_PORT", "18101");
    let control_router = Router::new()
        .route("/health", get(direct_health))
        .route("/await-durable-wait", post(direct_await_durable_wait));
"""

COMPOSE = """
services:
  worker-a:
    environment:
      WORKER_PORT: "18100"
"""


class CaddyfileTests(unittest.TestCase):
    def assert_fails(self, caddyfile: str, needle: str) -> None:
        with self.assertRaises(checker.CheckError) as caught:
            checker.verify_caddyfile(caddyfile)
        self.assertIn(needle, str(caught.exception))

    def test_fixed_configuration_passes(self) -> None:
        checker.verify_caddyfile(FIXED)

    def test_probing_the_invocation_port_fails(self) -> None:
        self.assert_fails(FIXED.replace("health_port 18101", "health_port 18100"), "18100")

    def test_dropping_health_port_fails(self) -> None:
        self.assert_fails(FIXED.replace("\t\thealth_port 18101\n", ""), "no 'health_port'")

    def test_probing_discover_fails(self) -> None:
        self.assert_fails(FIXED.replace("health_uri /health", "health_uri /discover"), "/discover")

    def test_dropping_health_checks_entirely_fails(self) -> None:
        stripped = FIXED.replace("\t\thealth_uri /health\n", "")
        self.assert_fails(stripped, "no 'health_uri'")

    def test_an_unrelated_health_port_fails(self) -> None:
        self.assert_fails(FIXED.replace("health_port 18101", "health_port 19999"), "18101")

    def test_a_second_health_port_fails(self) -> None:
        doubled = FIXED.replace(
            "\t\thealth_port 18101\n", "\t\thealth_port 18101\n\t\thealth_port 18100\n"
        )
        self.assert_fails(doubled, "expected once")

    def test_moving_the_invocation_upstreams_fails(self) -> None:
        self.assert_fails(FIXED.replace("worker-a:18100", "worker-a:18200"), "invocation upstreams")


class ControlListenerTests(unittest.TestCase):
    def assert_fails(self, worker_source: str, compose: str, needle: str) -> None:
        with self.assertRaises(checker.CheckError) as caught:
            checker.verify_control_listener(worker_source, compose)
        self.assertIn(needle, str(caught.exception))

    def test_matching_listener_passes(self) -> None:
        checker.verify_control_listener(WORKER_SOURCE, COMPOSE)

    def test_moving_the_control_port_fails(self) -> None:
        self.assert_fails(
            WORKER_SOURCE.replace('"WORKER_CONTROL_PORT", "18101"', '"WORKER_CONTROL_PORT", "18102"'),
            COMPOSE,
            "control listener",
        )

    def test_removing_the_health_route_fails(self) -> None:
        self.assert_fails(
            WORKER_SOURCE.replace('.route("/health", get(direct_health))\n', ""),
            COMPOSE,
            "GET /health",
        )

    def test_compose_overriding_the_control_port_fails(self) -> None:
        self.assert_fails(
            WORKER_SOURCE,
            COMPOSE + '      WORKER_CONTROL_PORT: "18999"\n',
            "overrides WORKER_CONTROL_PORT",
        )


class RepositoryTests(unittest.TestCase):
    def test_the_repository_harness_passes(self) -> None:
        checker.verify()


if __name__ == "__main__":
    unittest.main()
