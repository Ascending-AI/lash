#!/usr/bin/env python3
"""Convert Lash's chaos-soak duration syntax to a local runner timeout."""

from __future__ import annotations

import re
import sys


WATCHDOG_MARGIN_SECONDS = 10 * 60
MAX_TIMEOUT_SECONDS = (1 << 63) - 1


def timeout_seconds(value: str) -> int:
    """Parse the `SoakConfig` h/m/s/bare-seconds grammar and add cleanup time."""
    match = re.fullmatch(r"([0-9]+)([hms]?)", value.strip())
    if match is None:
        raise ValueError("expected <n>h, <n>m, <n>s, or bare seconds")
    multiplier = {"h": 3_600, "m": 60, "s": 1, "": 1}[match.group(2)]
    duration = int(match.group(1)) * multiplier
    timeout = duration + WATCHDOG_MARGIN_SECONDS
    if timeout > MAX_TIMEOUT_SECONDS:
        raise ValueError("duration is too large for the test runner")
    return timeout


def main() -> int:
    if len(sys.argv) != 2:
        print("usage: chaos_soak_timeout.py <duration>", file=sys.stderr)
        return 2
    try:
        print(timeout_seconds(sys.argv[1]))
    except ValueError as error:
        print(f"invalid chaos-soak duration {sys.argv[1]!r}: {error}", file=sys.stderr)
        return 2
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
