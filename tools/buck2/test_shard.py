#!/usr/bin/env python3
"""Run one balanced, exactly disjoint libtest shard in a single test process.

    test_shard.py COUNT INDEX [--weights TABLE LABEL] BINARY [--lash-libtest-args] [ARG...]

Every shard process of a test lists the whole binary and derives the same
assignment from the listed names and the committed weights table alone, so the
shards are disjoint and their union is the listing.

Exclusion is by exact name. The shard runs the binary under `--exact` with one
`--skip` for every listed case that is not its own: libtest applies `--exact`
to `--skip` as well as to filters, so a case whose name contains another's is
skipped or kept on its own.

libtest has one `--exact` for filters and skips alike, so the caller's own
filters cannot ride along beside the exact skips. They are applied here
instead, to the listing, by libtest's rule: a case is selected when it matches
a positional filter, if there is one, and no `--skip` pattern, where a match is
a substring, or equality under the caller's `--exact`. The filters and skips
are then dropped from the command and every case outside the selection is
skipped by name. A case therefore runs on the same shard whatever the filter.
Every other argument, `--ignored` and `--include-ignored` among them, reaches
libtest unchanged.
"""
import json
import re
import subprocess
import sys

from libtest_selection import ARGUMENT_MARKER, VALUE_FLAGS

# A feature-lane variant of a test shares its ordinary label's weights.
VARIANT = re.compile(r"__fv_[0-9a-f]+$")


def shard_assignments(tests: list[str], count: int, weights=None) -> dict[str, int]:
    """Assign each case a shard, balancing the weighted cases' total duration.

    Weighted cases go longest first, each to the shard with the least load so
    far. Cases the table does not know then go round-robin in name order,
    starting from the lightest shard. Ties break on the name and the shard
    index, so the result depends on nothing but the arguments.
    """
    weights = weights or {}
    names = sorted(set(tests))
    loads = [0] * count
    assignments = {}
    for name in sorted((name for name in names if name in weights), key=lambda name: (-weights[name], name)):
        shard = min(range(count), key=lambda shard: (loads[shard], shard))
        assignments[name] = shard
        loads[shard] += weights[name]
    order = sorted(range(count), key=lambda shard: (loads[shard], shard))
    for position, name in enumerate(name for name in names if name not in weights):
        assignments[name] = order[position % count]
    return assignments


def load_weights(path: str, label: str) -> dict[str, int]:
    """Return the table's milliseconds per case for `label`, or nothing for an unmeasured test."""
    with open(path, encoding="utf-8") as source:
        table = json.load(source)
    row = table.get(label, table.get(VARIANT.sub("", label), {}))
    if not isinstance(row, dict) or not all(
        isinstance(weight, int) and not isinstance(weight, bool) and weight >= 0 for weight in row.values()
    ):
        raise SystemExit(f"shard weights for {label} must map case names to milliseconds: {path}")
    return row


def selection(arguments: list[str]) -> tuple[list[str], list[str], bool, list[str]]:
    """Split libtest arguments into filters, skip patterns, `--exact` and the rest."""
    filters, skips, rest = [], [], []
    exact = False
    remaining = iter(arguments)
    for argument in remaining:
        if argument == "--skip":
            skips.extend(value for value in [next(remaining, None)] if value is not None)
        elif argument.startswith("--skip="):
            skips.append(argument.removeprefix("--skip="))
        elif argument == "--exact":
            exact = True
        elif argument in VALUE_FLAGS:
            rest.append(argument)
            rest.extend(value for value in [next(remaining, None)] if value is not None)
        elif argument.startswith("-"):
            rest.append(argument)
        else:
            filters.append(argument)
    return filters, skips, exact, rest


def selected(name: str, filters: list[str], skips: list[str], exact: bool) -> bool:
    def matches(pattern: str) -> bool:
        return name == pattern if exact else pattern in name

    return (not filters or any(map(matches, filters))) and not any(map(matches, skips))


def main() -> int:
    count = int(sys.argv[1])
    index = int(sys.argv[2])
    if count < 1 or index < 0 or index >= count:
        raise SystemExit("shard count must be positive and index must be in range")
    command = sys.argv[3:]
    weights = {}
    if command[:1] == ["--weights"]:
        if len(command) < 3:
            raise SystemExit("--weights takes a table and a test label")
        weights = load_weights(command[1], command[2])
        command = command[3:]
    if ARGUMENT_MARKER in command:
        marker = command.index(ARGUMENT_MARKER)
        command_prefix = command[:marker]
        arguments = command[marker + 1:]
    else:
        command_prefix = command[:1]
        arguments = command[1:]
    if not command_prefix:
        raise SystemExit("shard command is empty")
    listed = subprocess.run(
        [*command_prefix, "--list", "--format", "terse"],
        check=True,
        capture_output=True,
        text=True,
    )
    tests = [line.rsplit(": ", 1)[0] for line in listed.stdout.splitlines() if line.endswith(": test")]
    assignments = shard_assignments(tests, count, weights)
    filters, skips, exact, rest = selection(arguments)
    command = [*command_prefix, *rest, "--exact"]
    for name in tests:
        if assignments[name] != index or not selected(name, filters, skips, exact):
            command.extend(["--skip", name])
    return subprocess.run(command).returncode


if __name__ == "__main__":
    raise SystemExit(main())
