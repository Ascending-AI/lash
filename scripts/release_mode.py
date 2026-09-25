#!/usr/bin/env python3
"""The checked-in switch that pauses the durable-format bump gates until 1.0.

Before lash 1.0 nothing is in production and the clean-slate 1.0 release
resets every stored format, so a per-PR version bump protects nothing while
colliding on "main+1" across every open PR (FIG-3660). While
``tools/release-mode.toml`` carries ``pre_release = true``, each bump gate
reports "paused pre-1.0" and exits 0 instead of enforcing; the gates and
their logic stay in place and turn back on when the switch flips to
``false`` at the 1.0 cut, under the post-1.0 migration policy (ADR 0106).

Only the Python standard library is used, like the gates that read this.
"""

from __future__ import annotations

from pathlib import Path
import tomllib


# Resolved against the repository a gate is pointed at rather than the
# checkout the script sits in, so a test fixture carries its own switch.
RELEASE_MODE_FILE = Path("tools") / "release-mode.toml"


def pre_release(repo: Path) -> bool:
    """Whether ``repo``'s release mode pauses the durable-format bump gates.

    A missing or unreadable switch file answers ``False`` -- enforce, which
    is also the post-1.0 posture. A malformed file raises ``tomllib``'s
    decode error rather than silently pausing or silently enforcing.
    """
    try:
        text = (repo / RELEASE_MODE_FILE).read_text(encoding="utf-8")
    except OSError:
        return False
    return tomllib.loads(text).get("pre_release") is True
