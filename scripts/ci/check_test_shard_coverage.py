#!/usr/bin/env python3
"""Prove that a sharded test label's shards ran the whole binary (FIG-3572).

`rust_test`'s sharding wrapper assigns each case of a libtest binary to a
shard by name hash. `tools/bazel/postgres_slot_runner.sh` records, in each
shard's undeclared outputs, the binary's whole `--list` (`all.txt`) and the
cases that shard runs (`shard-<index>-of-<total>.txt`). For every label in
`--labels` whose `shard_count` in `tools/bazel/target-inventory.json` is set,
this checks that:

* every shard `shard_1_of_N` .. `shard_N_of_N` left its record,
* every shard saw the same, non-empty `--list`,
* no case runs in two shards, and
* the shards' union is the whole list.

A label with no `shard_count` is one test action running the whole binary,
and there is nothing to check. Bazel may zip undeclared outputs into
`test.outputs/outputs.zip`; both layouts are read.
"""

from __future__ import annotations

import argparse
import io
import json
import re
import sys
import zipfile
from pathlib import Path

ROOT = Path(__file__).resolve().parents[2]
INVENTORY = ROOT / "tools/bazel/target-inventory.json"
RECORD_DIR = "shard-coverage"
SHARD_RECORD = re.compile(r"shard-(\d+)-of-(\d+)\.txt")


def shard_counts(inventory: dict) -> dict[str, int]:
    counts: dict[str, int] = {}
    for package in inventory["packages"]:
        for target in package.get("targets", []):
            if target.get("shard_count"):
                counts[target["label"]] = int(target["shard_count"])
    return counts


def cases(text: str) -> list[str]:
    """The case names of libtest's `--list --format terse` output."""

    return [
        line[: -len(": test")]
        for line in text.splitlines()
        if line.endswith(": test")
    ]


def shard_records(outputs: Path) -> dict[str, str]:
    """File name -> text of one shard's coverage records."""

    records: dict[str, str] = {}
    directory = outputs / RECORD_DIR
    if directory.is_dir():
        for path in directory.iterdir():
            records[path.name] = path.read_text(encoding="utf-8")
    for archive in outputs.glob("*.zip"):
        with zipfile.ZipFile(archive) as zipped:
            for name in zipped.namelist():
                parts = Path(name).parts
                if len(parts) >= 2 and parts[-2] == RECORD_DIR:
                    with zipped.open(name) as handle:
                        records[parts[-1]] = io.TextIOWrapper(
                            handle, encoding="utf-8"
                        ).read()
    return records


def check_label(label: str, total: int, testlogs: Path) -> list[str]:
    package, _, name = label.removeprefix("//").partition(":")
    target_dir = testlogs / package / name
    errors: list[str] = []
    full: list[str] | None = None
    owner: dict[str, int] = {}
    for index in range(total):
        shard_dir = target_dir / f"shard_{index + 1}_of_{total}"
        records = shard_records(shard_dir / "test.outputs")
        where = f"{label} shard {index + 1}/{total}"
        if "all.txt" not in records:
            errors.append(f"{where} left no `--list` record under {shard_dir}")
            continue
        listed = cases(records["all.txt"])
        if not listed:
            errors.append(f"{where} listed no cases")
        if full is None:
            full = listed
        elif sorted(listed) != sorted(full):
            errors.append(f"{where} saw a different `--list` than shard 1")
        own = f"shard-{index}-of-{total}.txt"
        if own not in records:
            errors.append(f"{where} left no record of the cases it ran")
            continue
        for case in cases(records[own]):
            if case in owner:
                errors.append(
                    f"{label}: `{case}` ran in shards {owner[case] + 1} and {index + 1}"
                )
            owner[case] = index
        for name_ in records:
            match = SHARD_RECORD.fullmatch(name_)
            if match and (int(match[1]), int(match[2])) != (index, total):
                errors.append(f"{where} carries another shard's record {name_}")
    if full is not None:
        missing = sorted(set(full) - set(owner))
        extra = sorted(set(owner) - set(full))
        if missing:
            errors.append(
                f"{label}: {len(missing)} listed case(s) ran in no shard: "
                + ", ".join(missing[:10])
            )
        if extra:
            errors.append(
                f"{label}: shards ran case(s) the binary does not list: "
                + ", ".join(extra[:10])
            )
    return errors


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--labels", required=True, type=Path)
    parser.add_argument("--testlogs", required=True, type=Path)
    parser.add_argument("--inventory", type=Path, default=INVENTORY)
    args = parser.parse_args(argv)

    counts = shard_counts(json.loads(args.inventory.read_text(encoding="utf-8")))
    labels = [
        line.strip()
        for line in (ROOT / args.labels).read_text(encoding="utf-8").splitlines()
        if line.strip()
    ]
    errors: list[str] = []
    checked = []
    for label in labels:
        total = counts.get(label)
        if not total:
            continue
        checked.append(f"{label} ({total} shards)")
        errors.extend(check_label(label, total, args.testlogs))
    if errors:
        for error in errors:
            print(f"shard coverage: {error}", file=sys.stderr)
        return 1
    print(
        "shard coverage: every listed case ran in exactly one shard of "
        + (", ".join(checked) if checked else "no sharded label")
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
