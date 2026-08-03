#!/usr/bin/env python3
"""Require rustdoc on core-owned inherent members of lash-core root exports.

ADR 0051 makes those members integrator surface. The compiler's `missing_docs`
lint cannot express that boundary: enabling it for lash-core also rejects
thousands of unrelated modules, fields, variants, and facade-support details.
This check uses rustdoc's resolved export graph so aliases, visibility, and
inherent-vs-trait implementations are decided by the compiler.
"""

from __future__ import annotations

import argparse
import json
from pathlib import Path
from typing import Any


DOCUMENTED_MEMBER_KINDS = {"function", "assoc_const"}
ROOT_MEMBER_TYPE_KINDS = {"struct", "enum", "union"}


def item_kind(item: dict[str, Any]) -> str:
    return next(iter(item["inner"]))


def root_export_target(
    child: dict[str, Any], index: dict[str, Any]
) -> tuple[str, dict[str, Any]] | None:
    """Resolve a root export only when lash-core owns its target item."""
    if item_kind(child) == "use":
        target_id = child["inner"]["use"]["id"]
        name = child.get("name") or child["inner"]["use"]["source"].rsplit("::", 1)[-1]
    else:
        target_id = child["id"]
        name = child["name"]
    if target_id is None:
        return None
    target = index.get(str(target_id))
    if target is None:
        # A target absent from lash-core's index is defined by another crate.
        return None
    return name, target


def undocumented_root_members(document: dict[str, Any]) -> tuple[int, list[str]]:
    index = document["index"]
    root = index[str(document["root"])]["inner"]["module"]
    inspected = 0
    missing: list[str] = []

    for child_id in root["items"]:
        child = index[str(child_id)]
        if child["visibility"] != "public" or item_kind(child) == "module":
            continue
        resolved = root_export_target(child, index)
        if resolved is None:
            continue
        export_name, target = resolved
        target_kind = item_kind(target)
        if target_kind not in ROOT_MEMBER_TYPE_KINDS:
            continue

        for impl_id in target["inner"][target_kind].get("impls", []):
            impl_item = index.get(str(impl_id))
            if impl_item is None:
                continue
            implementation = impl_item["inner"].get("impl")
            if implementation is None or implementation["trait"] is not None:
                continue
            for member_id in implementation["items"]:
                member = index.get(str(member_id))
                if member is None or member["visibility"] != "public":
                    continue
                if item_kind(member) not in DOCUMENTED_MEMBER_KINDS:
                    continue
                inspected += 1
                if not (member.get("docs") or "").strip():
                    missing.append(f"{export_name}::{member['name']}")

    return inspected, sorted(set(missing))


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("rustdoc_json", type=Path)
    args = parser.parse_args()

    with args.rustdoc_json.open("rb") as source:
        document = json.load(source)
    inspected, missing = undocumented_root_members(document)
    if missing:
        print(
            "lash-core root-export inherent members missing integrator-class rustdoc:"
        )
        for symbol in missing:
            print(f"  {symbol}")
        return 1
    print(f"lash-core public member rustdoc: {inspected} documented, 0 missing")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
