#!/usr/bin/env python3
"""Print the Kiln executor runtime fingerprint this repository builds against.

`.bazelrc` is the single source of that fingerprint: it is the value
`build:shared` sends as `kiln_executor_runtime`, and the scheduler matches it
exactly, so a CI invocation that carried its own copy would silently stop
matching the pool the moment the image was rebuilt. CI therefore reads the
value from here instead of repeating it, and
`scripts/test_bazel_test_contract.py` refuses a second copy anywhere under
`.github/`.
"""
from __future__ import annotations

from pathlib import Path
import re
import sys

ROOT = Path(__file__).resolve().parents[1]
RUNTIME_PROPERTY = re.compile(
    r"^build:shared\s+--remote_default_exec_properties=kiln_executor_runtime=(\S+)\s*$",
    re.MULTILINE,
)


class FingerprintError(Exception):
    """The fingerprint could not be read from `.bazelrc`."""


def executor_runtime(bazelrc: str) -> str:
    """Return the one `kiln_executor_runtime` fingerprint `.bazelrc` declares."""
    matches = RUNTIME_PROPERTY.findall(bazelrc)
    if len(matches) != 1:
        raise FingerprintError(
            f"expected exactly one build:shared kiln_executor_runtime property, found {len(matches)}"
        )
    return matches[0]


def main() -> int:
    try:
        print(executor_runtime((ROOT / ".bazelrc").read_text(encoding="utf-8")))
    except (OSError, UnicodeError, FingerprintError) as error:
        print(f"Unable to read the executor runtime fingerprint: {error}", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
