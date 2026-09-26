#!/usr/bin/env python3
"""The checked-in pre-1.0 freeze for the durable-format version-bump gates.

Before lash 1.0 nothing is in production and the clean-slate 1.0 release
resets every stored format, so a per-PR version bump protects nothing while
colliding on "main+1" across every open PR (FIG-3660, FIG-3846). While the
surface inventory's top-level ``[policy]`` table carries
``freeze = "pre-1.0"``, each bump gate still runs, prints the findings it
would enforce, prints ``FREEZE_NOTICE``, and exits 0; the checks and their
logic stay in place and turn back on when the key is removed at the 1.0 cut,
under the post-1.0 migration policy (ADR 0106).

Only the Python standard library is used, like the gates that read this.
"""

from __future__ import annotations

from pathlib import Path
import tomllib


# Resolved against the repository a gate is pointed at rather than the
# checkout the script sits in, so a test fixture carries its own switch.
SURFACES_FILE = Path("scripts") / "versioned-surfaces.toml"

# The only freeze the policy knows. Any other value -- and a missing or
# differently-shaped ``[policy]`` table -- means enforce, which is also the
# post-1.0 posture.
FREEZE = "pre-1.0"

# The one line every gate prints while the freeze reports its findings
# instead of enforcing them.
FREEZE_NOTICE = (
    "version freeze active (pre-1.0): findings reported, not enforced"
)


def declared_freeze(inventory: Path) -> str | None:
    """The ``freeze`` an inventory's top-level ``[policy]`` table declares.

    A missing or unreadable inventory answers ``None`` -- enforce, which is
    also the post-1.0 posture. A malformed file raises ``tomllib``'s decode
    error rather than silently freezing or silently enforcing.
    """
    try:
        document = tomllib.loads(inventory.read_text(encoding="utf-8"))
    except OSError:
        return None
    policy = document.get("policy")
    if not isinstance(policy, dict):
        return None
    freeze = policy.get("freeze")
    return freeze if isinstance(freeze, str) else None


def frozen(repo: Path) -> bool:
    """Whether ``repo``'s surface inventory freezes version bumps pre-1.0."""
    return declared_freeze(repo / SURFACES_FILE) == FREEZE
