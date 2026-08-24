#!/usr/bin/env python3
"""Verify Restate workers E2E shard manifests cover the runner inventory."""

from __future__ import annotations

import argparse
from dataclasses import dataclass
from pathlib import Path
import sys


@dataclass(frozen=True)
class Leg:
    segment: int
    inventory_path: Path
    completed_path: Path


def _read_inventory(path: Path) -> dict[str, int]:
    inventory: dict[str, int] = {}
    for line_number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        fields = raw.split("\t")
        if len(fields) != 2 or fields[0] not in {"1", "2"} or not fields[1]:
            raise ValueError(f"{path}:{line_number}: expected '<segment>\\t<workflow-id>'")
        workflow_id = fields[1]
        if workflow_id in inventory:
            raise ValueError(f"{path}:{line_number}: duplicate workflow '{workflow_id}'")
        inventory[workflow_id] = int(fields[0])
    if not inventory:
        raise ValueError(f"{path}: workflow inventory is empty")
    return inventory


def _read_completed(path: Path) -> set[str]:
    workflows: set[str] = set()
    for line_number, raw in enumerate(path.read_text(encoding="utf-8").splitlines(), 1):
        workflow_id = raw.strip()
        if not workflow_id:
            raise ValueError(f"{path}:{line_number}: blank workflow id")
        if workflow_id in workflows:
            raise ValueError(f"{path}:{line_number}: duplicate workflow '{workflow_id}'")
        workflows.add(workflow_id)
    if not workflows:
        raise ValueError(f"{path}: completed-workflow manifest is empty")
    return workflows


def verify(legs: list[Leg]) -> None:
    by_segment = {leg.segment: leg for leg in legs}
    if len(by_segment) != len(legs):
        raise ValueError("each segment may be supplied only once")
    if set(by_segment) != {1, 2}:
        raise ValueError(f"expected completed manifests for segments 1 and 2, got {sorted(by_segment)}")

    inventories = [_read_inventory(by_segment[segment].inventory_path) for segment in (1, 2)]
    if inventories[0] != inventories[1]:
        raise ValueError("runner workflow inventories differ between CI legs")
    inventory = inventories[0]

    completed_by_segment = {
        segment: _read_completed(by_segment[segment].completed_path) for segment in (1, 2)
    }
    for segment, completed in completed_by_segment.items():
        expected = {
            workflow_id
            for workflow_id, owner in inventory.items()
            if owner == segment
        }
        missing = sorted(expected - completed)
        unexpected = sorted(completed - expected)
        if missing or unexpected:
            raise ValueError(
                f"segment {segment} coverage mismatch: missing={missing}, unexpected={unexpected}"
            )

    union = completed_by_segment[1] | completed_by_segment[2]
    if union != set(inventory):
        raise ValueError(
            "completed-workflow union does not equal the authoritative runner inventory"
        )


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--leg",
        action="append",
        nargs=3,
        metavar=("SEGMENT", "INVENTORY", "COMPLETED"),
        default=[],
    )
    args = parser.parse_args(argv)
    try:
        legs = [
            Leg(int(segment), Path(inventory), Path(completed))
            for segment, inventory, completed in args.leg
        ]
        verify(legs)
    except (OSError, ValueError) as error:
        print(f"Restate workers E2E coverage: FAILED: {error}", file=sys.stderr)
        return 1
    print("Restate workers E2E coverage: passed (segments 1 + 2 equal runner inventory)")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
