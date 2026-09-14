#!/usr/bin/env python3
"""Pin the workers-E2E proxy's active health check to the worker control port.

Caddy's active health checker shares the reverse proxy's HTTP transport, and Go
keys HTTP/2 connections by authority. A probe aimed at the Restate invocation
port therefore rides the *same* h2c connection as `E2eTurnWorkflow/run` and
`LashDurableWaitWorkflow/await_resolution`. Each probe that outlives
`health_timeout` is cancelled, and a cancelled stream the worker has not yet
accepted counts against h2's pending-accept rapid-reset budget (default 20).
Past that budget the worker answers with a *connection-level* GOAWAY
`ENHANCE_YOUR_CALM debug="too_many_resets"`, which aborts every durable
invocation multiplexed on that connection. That is FIG-3062: 158 cancelled
probes tore down 5 connections and 12 in-flight invocations in one CI run, and
segment 1 never converged.

The worker runs a second, independent listener for exactly this traffic — the
axum control endpoint on `WORKER_CONTROL_PORT` with a `GET /health` route — so
the probe belongs there, on its own connection, where a cancellation cannot
reach an invocation stream.

This check fails if anyone points the health target back at the invocation port
or at the Restate discovery path.
"""

from __future__ import annotations

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
HARNESS = ROOT / "runbooks" / "restate-postgres-workers"
CADDYFILE = HARNESS / "Caddyfile"
COMPOSE = HARNESS / "docker-compose.yml"
WORKER_SOURCE = HARNESS / "src" / "bin" / "worker.rs"

INVOCATION_PORT = "18100"
CONTROL_PORT = "18101"
HEALTH_URI = "/health"


class CheckError(Exception):
    """A defect in the harness proxy configuration."""


def _directive(caddyfile: str, name: str) -> str | None:
    """Return the single argument of `name`, or None when it is absent."""
    matches = re.findall(rf"^\s*{re.escape(name)}\s+(\S+)\s*$", caddyfile, re.MULTILINE)
    if not matches:
        return None
    if len(matches) > 1:
        raise CheckError(f"{CADDYFILE.name}: '{name}' is set {len(matches)} times; expected once")
    return matches[0]


def verify_caddyfile(caddyfile: str) -> None:
    upstream_ports = set(re.findall(r"h2c://worker-[ab]:(\d+)", caddyfile))
    if upstream_ports != {INVOCATION_PORT}:
        raise CheckError(
            f"{CADDYFILE.name}: expected the invocation upstreams on port {INVOCATION_PORT}, "
            f"found {sorted(upstream_ports) or 'none'}"
        )

    health_uri = _directive(caddyfile, "health_uri")
    if health_uri is None:
        raise CheckError(
            f"{CADDYFILE.name}: no 'health_uri'. Active health checks are how the proxy routes "
            "away from a crashed worker (FIG-3030); do not drop them."
        )
    if health_uri != HEALTH_URI:
        raise CheckError(
            f"{CADDYFILE.name}: health_uri is '{health_uri}', expected '{HEALTH_URI}'. "
            "'/discover' is served by the Restate endpoint on the invocation connection; "
            "probing it there lets a cancelled probe GOAWAY every live invocation (FIG-3062)."
        )

    health_port = _directive(caddyfile, "health_port")
    if health_port is None:
        raise CheckError(
            f"{CADDYFILE.name}: no 'health_port'. Without it the probe inherits the upstream port "
            f"{INVOCATION_PORT} and shares the invocation connection (FIG-3062); pin it to the "
            f"worker control port {CONTROL_PORT}."
        )
    if health_port == INVOCATION_PORT:
        raise CheckError(
            f"{CADDYFILE.name}: health_port {INVOCATION_PORT} is the invocation port. A cancelled "
            "probe there counts against the h2 pending-accept reset budget and takes the whole "
            "connection down with it (FIG-3062)."
        )
    if health_port != CONTROL_PORT:
        raise CheckError(
            f"{CADDYFILE.name}: health_port is '{health_port}', expected the worker control port "
            f"{CONTROL_PORT}."
        )


def verify_control_listener(worker_source: str, compose: str) -> None:
    """The probe target has to exist, on its own listener, at the pinned port."""
    if f'env("WORKER_CONTROL_PORT", "{CONTROL_PORT}")' not in worker_source:
        raise CheckError(
            f"{WORKER_SOURCE.name}: the worker control listener no longer defaults to port "
            f"{CONTROL_PORT}; the proxy health check is pinned to it."
        )
    if f'.route("{HEALTH_URI}", get(' not in worker_source:
        raise CheckError(
            f"{WORKER_SOURCE.name}: the control router no longer serves GET {HEALTH_URI}; "
            "the proxy health check targets it."
        )
    if "WORKER_CONTROL_PORT" in compose:
        raise CheckError(
            "docker-compose.yml overrides WORKER_CONTROL_PORT; the health check is pinned to "
            f"{CONTROL_PORT} and would probe a port nothing listens on."
        )


def verify(root: Path = ROOT) -> None:
    harness = root / "runbooks" / "restate-postgres-workers"
    verify_caddyfile((harness / "Caddyfile").read_text(encoding="utf-8"))
    verify_control_listener(
        (harness / "src" / "bin" / "worker.rs").read_text(encoding="utf-8"),
        (harness / "docker-compose.yml").read_text(encoding="utf-8"),
    )


def main() -> int:
    try:
        verify()
    except (CheckError, OSError) as error:
        print(f"restate workers proxy health target check failed: {error}", file=sys.stderr)
        return 1
    print(
        "restate workers proxy health target check passed: active health checks probe "
        f"{HEALTH_URI} on the control port {CONTROL_PORT}, off the invocation connection"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
