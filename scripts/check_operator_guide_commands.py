#!/usr/bin/env python3
"""Require each lashctl command in the operator guide to run in the rolling runbook.

Only fenced commands count in the guide. A guide command is exempt when
the line immediately above it inside the same fence is exactly
``# after-1.0``. That marker is for a command explicitly identified as
unavailable in the 1.0 binary. In the runbook's Operator commands table, a
verb counts only when its "Runs today as" cell names the same lashctl verb.
Rows marked "not yet" or delegated to lash-upgrade-node do not count.
"""

from __future__ import annotations

from pathlib import Path
import re
import sys


ROOT = Path(__file__).resolve().parents[1]
GUIDE = ROOT / "docs/operations/deploying-and-upgrading.md"
RUNBOOKS = ROOT / "runbooks/rolling-upgrade"
COMMAND = re.compile(r"(?<![\w-])lashctl\s+([a-z][a-z0-9-]*)\b")
TABLE_COMMAND = re.compile(r"\blashctl\s+([a-z][a-z0-9-]*)\b")
AFTER_1_0 = "# after-1.0"


def shell_commands(source: str) -> set[str]:
    commands: set[str] = set()
    in_shell = False
    previous = ""
    for line in source.splitlines():
        if not in_shell:
            if re.match(r"^\s*```[^`]*$", line):
                in_shell = True
                previous = ""
            continue
        if re.match(r"^\s*```\s*$", line):
            in_shell = False
            continue
        if previous != AFTER_1_0 and not line.lstrip().startswith("#"):
            commands.update(COMMAND.findall(line))
        previous = line.strip()
    return commands


def runbook_commands(source: str) -> set[str]:
    commands: set[str] = set()
    in_table = False
    for line in source.splitlines():
        if line.startswith("## "):
            in_table = line == "## Operator commands"
        if not in_table or not line.startswith("|"):
            continue
        cells = [cell.strip() for cell in line.strip("|\n").split("|")]
        if len(cells) != 4:
            continue
        named = TABLE_COMMAND.search(cells[0])
        runs_as = TABLE_COMMAND.search(cells[3])
        if (
            named
            and runs_as
            and named.group(1) == runs_as.group(1)
            and "not yet" not in cells[3].lower()
            and "lash-upgrade-node" not in cells[3]
        ):
            commands.add(named.group(1))
    return commands


def main() -> int:
    try:
        guide = shell_commands(GUIDE.read_text(encoding="utf-8"))
        runbook = set().union(
            *(
                runbook_commands(path.read_text(encoding="utf-8"))
                for path in RUNBOOKS.rglob("*.md")
            )
        )
    except OSError as error:
        print(f"operator guide command check: {error}", file=sys.stderr)
        return 1
    if not guide:
        print("operator guide command check: no lashctl commands in guide shell blocks", file=sys.stderr)
        return 1
    missing = sorted(guide - runbook)
    if missing:
        print("operator guide commands missing from rolling runbook steps:", file=sys.stderr)
        for verb in missing:
            print(f"- lashctl {verb}", file=sys.stderr)
        return 1
    print(f"operator guide commands covered: {len(guide)} verbs ({', '.join(sorted(guide))})")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
