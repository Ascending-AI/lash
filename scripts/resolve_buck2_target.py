#!/usr/bin/env python3
"""Resolve one generated Buck2 feature target from the checked-in inventory."""

from __future__ import annotations

import argparse
import json
from pathlib import Path


ROOT = Path(__file__).resolve().parents[1]


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("package", help="Buck2 package, such as //crates/lashctl")
    parser.add_argument("target", help="generated target basename before __fv_ suffix")
    parser.add_argument("--feature", action="append", default=[])
    args = parser.parse_args()
    inventory = json.loads(
        (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
    )
    prefix = f"{args.package}:{args.target}__fv_"
    wanted = sorted(args.feature)
    matches = sorted(
        {
            unit["label"]
            for unit in inventory["feature_lane_units"]
            if unit.get("label", "").startswith(prefix)
            and sorted(unit.get("features", ())) == wanted
        }
    )
    if len(matches) != 1:
        parser.error(
            f"expected one {prefix} target with features {wanted}, found {matches}"
        )
    print(matches[0])
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
