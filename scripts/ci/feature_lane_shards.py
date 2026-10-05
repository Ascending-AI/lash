#!/usr/bin/env python3
"""Partition the feature-lane targets across the `feature-lanes` matrix shards.

One runner building `//:feature_lane_compile` ran past its 45-minute limit:
the lane graph is some nine hundred variant units, and a single invocation
feeds the pool at one client's concurrency. The job is a matrix now, and this
module decides which shard owns which target.

The unit of partition is the coverage lane, not the target. A lane is one
resolution of one package's closure, so its units depend on each other and on
little else: keeping a lane whole lets a shard build each dependency variant
once, where a per-target split made every shard rebuild most of the graph.
Lanes are placed heaviest first on the lightest shard; a target several lanes
share belongs to the first of them placed, so every target has exactly one
owner.

A target's weight is its compile reservation (`cpu_count * memory_kb`) from
`tools/buck2/action-sizes.json`, the measured table the generator sizes the
same actions from; an unmeasured crate weighs the default small action.

The partition is a pure function of two checked-in files, so every shard of a
run computes the same one. `scripts/test_feature_lane_shards.py` proves it is
complete and disjoint for the real target lists.

    feature_lane_shards.py <shard> <group> -- <command...>

runs the command with the shard's labels of that group appended, or does
nothing when the shard owns none of them.
"""

from __future__ import annotations

import json
import os
import pathlib
import sys

ROOT = pathlib.Path(__file__).resolve().parents[2]
INVENTORY = ROOT / "tools" / "buck2" / "target-inventory.json"
ACTION_SIZES = ROOT / "tools" / "buck2" / "action-sizes.json"

# The matrix in .github/workflows/ci.yml lists exactly 1..SHARDS. Four is where
# the heaviest lane alone becomes the largest shard, so a fifth would not
# shorten the slowest leg.
SHARDS = 4

# The aggregate each lane step used to build whole, and the inventory list
# that holds its members.
GROUPS = {
    "//:feature_lane_compile": "feature_lane_compile_targets",
    "//:feature_lane_tests": "feature_lane_test_targets",
}

# The small action of tools/buck2/platforms.bzl: 1 CPU, 1.5 GiB.
DEFAULT_WEIGHT = 1 * 1572864


def load() -> tuple[dict, dict]:
    return (
        json.loads(INVENTORY.read_text(encoding="utf-8")),
        json.loads(ACTION_SIZES.read_text(encoding="utf-8")),
    )


def weights(inventory: dict, sizes: dict) -> dict[str, int]:
    """Each lane target's compile reservation, by label."""
    crates = {
        target["label"]: package["package"] + "/" + target["cargo"]
        for package in inventory["packages"]
        for target in package["targets"]
        if target.get("label") and "cargo" in target
    }
    result = {}
    for labels in inventory["feature_lanes"].values():
        for label in labels:
            row = sizes.get(crates.get(label.split("__fv_", 1)[0], ""))
            result[label] = (
                row["cpu_count"] * row["memory_kb"] if row else DEFAULT_WEIGHT
            )
    return result


def placement(
    inventory: dict, sizes: dict, shards: int = SHARDS
) -> tuple[dict[str, int], dict[str, int]]:
    """Place each lane on a shard; return the target owners and the lane placements."""
    lanes = inventory["feature_lanes"]
    weight = weights(inventory, sizes)
    owner: dict[str, int] = {}
    placed: dict[str, int] = {}
    load_of = [0] * shards
    for lane in sorted(
        lanes, key=lambda name: (-sum(weight[label] for label in lanes[name]), name)
    ):
        fresh = [label for label in lanes[lane] if label not in owner]
        shard = min(range(shards), key=lambda index: (load_of[index], index))
        placed[lane] = shard + 1
        for label in fresh:
            owner[label] = shard + 1
        load_of[shard] += sum(weight[label] for label in fresh)
    unowned = sorted(
        label
        for group in GROUPS.values()
        for label in inventory[group]
        if label not in owner
    )
    if unowned:
        raise ValueError(
            "feature-lane targets outside every lane cannot be sharded: "
            + ", ".join(unowned)
        )
    return owner, placed


def partition(inventory: dict, sizes: dict, shards: int = SHARDS) -> dict[str, int]:
    """Map every feature-lane target to the one shard (1..shards) that owns it."""
    return placement(inventory, sizes, shards)[0]


def selection(inventory: dict, sizes: dict, group: str, shard: int) -> list[str]:
    """The shard's members of one aggregate, in the aggregate's own order."""
    if group not in GROUPS:
        raise ValueError(f"unknown feature-lane group {group}; expected one of {sorted(GROUPS)}")
    owner = partition(inventory, sizes)
    return [label for label in inventory[GROUPS[group]] if owner[label] == shard]


def shard_floors(inventory: dict, sizes: dict, shard: int) -> dict[str, int]:
    """The test-case floors whose targets the shard owns and so executes."""
    owner = partition(inventory, sizes)
    return {
        label: floor
        for label, floor in inventory["feature_lane_test_floors"].items()
        if owner[label] == shard
    }


def parse_shard(value: str) -> int:
    if not value.isdecimal() or not 1 <= int(value) <= SHARDS:
        raise ValueError(f"shard must be 1..{SHARDS}, not {value!r}")
    return int(value)


def summary(inventory: dict, sizes: dict) -> str:
    owner, placed = placement(inventory, sizes)
    weight = weights(inventory, sizes)
    total = sum(weight.values())
    lines = []
    for shard in range(1, SHARDS + 1):
        counts = " ".join(
            f"{group.removeprefix('//:feature_lane_')}="
            f"{sum(owner[label] == shard for label in inventory[key])}"
            for group, key in GROUPS.items()
        )
        share = 100 * sum(weight[label] for label in owner if owner[label] == shard) / total
        lanes = ",".join(sorted(lane for lane in placed if placed[lane] == shard))
        lines.append(f"shard {shard}/{SHARDS}: {counts} weight={share:.1f}% lanes={lanes}")
    return "\n".join(lines)


def main(argv: list[str]) -> int:
    inventory, sizes = load()
    if argv == ["summary"]:
        print(summary(inventory, sizes))
        return 0
    if len(argv) < 4 or argv[2] != "--":
        print(
            "usage: feature_lane_shards.py <shard> <group> -- <command...>\n"
            "       feature_lane_shards.py summary",
            file=sys.stderr,
        )
        return 2
    shard = parse_shard(argv[0])
    labels = selection(inventory, sizes, argv[1], shard)
    if not labels:
        print(f"feature-lane shard {shard}/{SHARDS} owns no {argv[1]} targets")
        return 0
    print(
        f"feature-lane shard {shard}/{SHARDS}: {len(labels)} of"
        f" {len(inventory[GROUPS[argv[1]]])} {argv[1]} targets",
        flush=True,
    )
    os.chdir(ROOT)
    os.execvp(argv[3], argv[3:] + labels)


if __name__ == "__main__":
    try:
        raise SystemExit(main(sys.argv[1:]))
    except ValueError as error:
        raise SystemExit(f"feature_lane_shards: {error}")
