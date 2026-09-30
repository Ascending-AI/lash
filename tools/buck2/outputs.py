#!/usr/bin/env python3
"""Resolve materialized artifacts from a Buck2 build report."""
import argparse
import json
from pathlib import Path


def resolve(report, label, subtarget=None):
    label = "root" + label if label.startswith("//") else label
    result = report.get("results", {}).get(label)
    if result is None:
        raise ValueError(f"The build report has no result for {label}")
    if result.get("success") != "SUCCESS":
        raise ValueError(f"The build did not succeed for {label}")
    groups = result.get("outputs", {})
    if subtarget is not None:
        if subtarget not in groups:
            raise ValueError(f"The build report has no {subtarget!r} output for {label}")
        groups = {subtarget: groups[subtarget]}
    root = Path(report["project_root"])
    paths = sorted({str((root / path).resolve()) for values in groups.values() for path in values})
    for path in paths:
        if not Path(path).exists():
            raise ValueError(f"Build output has not been materialized: {path}; rebuild with --materializations final")
    return paths


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--report", required=True, type=Path)
    parser.add_argument("--label", required=True)
    parser.add_argument("--subtarget")
    parser.add_argument("--single", action="store_true")
    args = parser.parse_args()
    try:
        paths = resolve(json.loads(args.report.read_text()), args.label, args.subtarget)
        if args.single:
            if len(paths) != 1:
                raise ValueError(f"Expected one output, found {len(paths)}")
            print(paths[0])
        else:
            print(json.dumps(paths))
    except (OSError, ValueError, KeyError) as error:
        parser.exit(1, f"outputs: {error}\n")


if __name__ == "__main__":
    main()
