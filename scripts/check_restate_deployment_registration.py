#!/usr/bin/env python3
"""Refuse a raw Restate deployment registration outside `crates/lash-restate`.

`RestateEngine::register_deployment` (FIG-3898, ADR 0111) checks every stable
Lash service name for a colliding authority before it posts to the admin
API's `/deployments` collection, so a host that registers itself any other
way can silently take over another deployment's calls. FIG-3912 moved every
host onto that method; this gate keeps a raw `POST .../deployments` from
sneaking back into a host, launcher, or test fixture.

What counts: a POST token (`.post(`, `HttpRequest::post`, `requests.post`,
`-X POST`, `--request POST`, `Method::POST`, `HttpMethod::Post`) inside a
short line window that also spells `deployments`. A read — `GET
/deployments`, the `deployment_registry_records` listing the workbench
launcher reads back, `sys_deployment` queries — names no POST token, so the
read-only diagnostics stay legal.

Exempt, each with the reason it is not a host bypassing the guard:

* `crates/lash-restate/` — the engine's own admin client, where the guarded
  POST lives.
* `crates/lash-restate-test/` — the engine test crate, which probes the
  Restate server's raw admin semantics on purpose (the same boundary
  `check-substrate-boundary.sh` draws).
* `scripts/test-agent-workbench-dev-reset.sh` — the launcher self-test: its
  mock curl implements the fake admin server's `POST /deployments` handler,
  and its mock workbench binary emulates the engine's own request so the
  launcher's retries and fault handling run against the same surface.
* This check and its self-test — they carry violating samples.
"""

from __future__ import annotations

from pathlib import Path
import re
import subprocess
import sys
from typing import Iterator


ROOT = Path(__file__).resolve().parents[1]

SCAN_SUFFIXES = frozenset({".rs", ".sh", ".bash", ".py"})
SCAN_NAMES = frozenset({"justfile", "Justfile"})

EXEMPT_PREFIXES = ("crates/lash-restate/", "crates/lash-restate-test/")
EXEMPT_FILES = frozenset(
    {
        "scripts/check_restate_deployment_registration.py",
        "scripts/test_check_restate_deployment_registration.py",
        "scripts/test-agent-workbench-dev-reset.sh",
    }
)

POST_TOKEN = re.compile(
    r"\.post\s*\(|HttpRequest::post\b|requests\.post\s*\(|-X\s*POST\b"
    r"|--request[=\s]+POST\b|\bMethod::POST\b|\bHttpMethod::Post\b"
)
# `deployments` as a path segment or string literal: `/deployments`,
# `"deployments"`, `deployments/`. The word in a comment or identifier
# (`deployment_registry_records`) does not match.
DEPLOYMENTS = re.compile(r"[/\"]deployments(?:[/\"]|$)")

# A registration posts a URL it built a line or two away; the window reaches
# both directions so `let url = format!("{}/deployments", ...)` followed by
# `.post(&url)` flags as surely as the trailing curl argument does.
WINDOW = 6


class CheckError(Exception):
    """A raw Restate deployment registration outside the engine."""


def scanned_paths(repo: Path) -> Iterator[Path]:
    listing = subprocess.run(
        ["git", "-C", str(repo), "ls-files", "-z"],
        check=True,
        capture_output=True,
        text=True,
    ).stdout
    for name in listing.split("\0"):
        if not name:
            continue
        path = Path(name)
        if name.startswith(EXEMPT_PREFIXES) or name in EXEMPT_FILES:
            continue
        if path.suffix in SCAN_SUFFIXES or path.name in SCAN_NAMES:
            yield path


def find_violations(text: str) -> list[int]:
    """The 1-based line numbers where a POST reaches `/deployments`."""
    lines = text.splitlines()
    violations: list[int] = []
    for index, line in enumerate(lines):
        if not POST_TOKEN.search(line):
            continue
        window = lines[max(0, index + 1 - WINDOW) : index + WINDOW]
        if any(DEPLOYMENTS.search(candidate) for candidate in window):
            violations.append(index + 1)
    return violations


def verify(root: Path = ROOT) -> None:
    failures: list[str] = []
    for name in scanned_paths(root):
        path = root / name
        try:
            text = path.read_text(encoding="utf-8")
        except (OSError, UnicodeDecodeError):
            continue
        for line in find_violations(text):
            failures.append(f"{name}:{line}")
    if failures:
        raise CheckError(
            "raw Restate deployment registration found outside crates/lash-restate:\n"
            + "\n".join(failures)
            + "\nregister through `RestateEngine::register_deployment` so the "
            "namespace collision guard applies (ADR 0111)"
        )


def main() -> int:
    try:
        verify()
    except (CheckError, OSError, subprocess.CalledProcessError) as error:
        print(f"restate deployment registration check failed: {error}", file=sys.stderr)
        return 1
    print(
        "restate deployment registration check passed: no raw POST to a Restate "
        "/deployments endpoint outside crates/lash-restate"
    )
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
