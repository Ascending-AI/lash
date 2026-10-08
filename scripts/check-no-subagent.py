#!/usr/bin/env python3
"""Refuse any subagent concept in lash's core crates and its facade (FIG-5296).

Creating a session is explicit and only a fork clones (ADR 0134). The core
knows sessions, an optional parent link and forks; what a delegated child is,
what it inherits and what context it carries belong to the host's own plugin.
Lash ships no subagent implementation: `examples/delegation` shows how a host
builds one on the facade.

The check fails on every case-insensitive `subagent` in a tracked path or in a
tracked text file's contents under the core crates (`crates/lash-core*/`),
`crates/lash-sansio/`, `crates/lash-durable/` and the facade (`crates/lash/`).
There is no exemption: the word may appear in examples and docs only.

Run it from anywhere: `python3 scripts/check-no-subagent.py`.
"""

from __future__ import annotations

import argparse
from dataclasses import dataclass
import re
import subprocess
import sys
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]
MENTION = re.compile("subagent", re.I)
# A path is guarded when its first two components name a guarded crate.
GUARDED_CRATE = re.compile(r"^crates/(lash-core[^/]*|lash-sansio|lash-durable|lash)/")


@dataclass(frozen=True)
class Mention:
    path: str
    line: int
    text: str

    def render(self) -> str:
        where = f"{self.path}:{self.line}" if self.line else self.path
        first = MENTION.search(self.text)
        assert first is not None
        start = max(first.start() - 60, 0)
        return f"{where}: a subagent concept in core: {self.text[start:first.end() + 60].strip()}"


def git(root: Path, *args: str) -> str:
    completed = subprocess.run(["git", *args], cwd=root, check=False,
                               capture_output=True, text=True, errors="replace")
    # `git grep` exits 1 when nothing matches.
    if completed.returncode not in (0, 1):
        raise SystemExit(f"git {' '.join(args[:2])} failed: {completed.stderr.strip()}")
    return completed.stdout


def check(root: Path) -> list[str]:
    """Every mention in a guarded tracked path or a guarded tracked text file."""
    found = [Mention(path, 0, path)
             for path in git(root, "ls-files", "-z").split("\0")
             if GUARDED_CRATE.match(path) and MENTION.search(path)]
    for record in git(root, "grep", "-I", "-n", "-i", "-z", "-e", "subagent").splitlines():
        path, line, text = record.split("\0", 2)
        if GUARDED_CRATE.match(path):
            found.append(Mention(path, int(line), text))
    return [mention.render() for mention in found]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--root", type=Path, default=ROOT, help="repository root to check")
    args = parser.parse_args()
    findings = check(args.root.resolve())
    for finding in findings:
        print(finding, file=sys.stderr)
    if findings:
        print(f"check-no-subagent: {len(findings)} mention(s); core and the facade have no "
              "subagent concept (ADR 0134). Delegation belongs to the host's plugin; see "
              "examples/delegation.", file=sys.stderr)
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
