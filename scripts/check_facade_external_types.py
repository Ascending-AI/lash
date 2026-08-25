#!/usr/bin/env python3
"""Reject unacknowledged third-party types in the Lash facade signatures."""

from __future__ import annotations

import argparse
from pathlib import Path
import sys
import tomllib
from typing import Any, Iterator


SCRIPTS = Path(__file__).resolve().parent
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

import api_surface  # noqa: E402
import check_api_example_coverage as api_coverage  # noqa: E402


ALLOWLIST = SCRIPTS / "facade-external-types.toml"
STANDARD_CRATES = {"alloc", "core", "proc_macro", "std", "test"}


def public_exports(
    document: dict[str, Any], module_id: str | None = None
) -> Iterator[dict[str, Any]]:
    """Yield each non-module public export in the facade module tree."""
    index = document["index"]
    current = str(document["root"] if module_id is None else module_id)
    for child_id in index[current]["inner"]["module"]["items"]:
        child = index[str(child_id)]
        if not api_surface.public(child["visibility"]):
            continue
        if api_surface.item_kind(child) == "module":
            yield from public_exports(document, str(child_id))
        else:
            yield child


def resolved_export(
    item: dict[str, Any], document: dict[str, Any], all_features: bool
) -> tuple[dict[str, Any], dict[str, Any]]:
    """Resolve a facade export to the document that owns its definition."""
    item_id = api_surface.target_id(item)
    if item_id is None:
        return item, document
    local = document["index"].get(item_id)
    if local is not None:
        return local, document
    path = document["paths"].get(item_id)
    if path is None:
        source = item["inner"].get("use", {}).get("source", item.get("name"))
        raise RuntimeError(
            f"facade export {source!r} targets item {item_id}, which has no rustdoc path"
        )
    resolved = api_surface.external_target(path, all_features)
    if resolved is None:
        canonical = "::".join(path.get("path") or [f"item {item_id}"])
        feature_set = "all features" if all_features else "default features"
        raise RuntimeError(
            f"cannot resolve external facade export {canonical} under {feature_set}: "
            "dependency rustdoc is unavailable or does not contain the target"
        )
    return resolved


def exposed_references(
    export: dict[str, Any], document: dict[str, Any], all_features: bool
) -> Iterator[tuple[str, dict[str, Any]]]:
    """Yield signature type ids from an export, recursing through modules."""
    item, owner = resolved_export(export, document, all_features)
    if api_surface.item_kind(item) != "module":
        for referenced_id in api_surface.exposed_ids(str(item["id"]), owner["index"]):
            yield referenced_id, owner
        return

    module = item.get("inner", {}).get("module")
    if not isinstance(module, dict) or not isinstance(module.get("items"), list):
        raise RuntimeError(
            f"cannot recurse external facade module export {item.get('name')!r}"
        )
    for child_id in module["items"]:
        child = owner["index"].get(str(child_id))
        if child is None:
            raise RuntimeError(
                "cannot recurse external facade module export "
                f"{item.get('name')!r}: missing child item {child_id}"
            )
        if api_surface.public(child["visibility"]):
            yield from exposed_references(child, owner, all_features)


def external_types(all_features: bool) -> set[str]:
    """Return canonical third-party type paths exposed by facade signatures."""
    document = api_surface.rustdoc("lash-runtime", "lash", all_features)
    workspace_crates = set(api_coverage.crate_directories()) | {"lash"}
    leaked: set[str] = set()

    for export in public_exports(document):
        for referenced_id, owner in exposed_references(
            export, document, all_features
        ):
            entry = owner["paths"].get(referenced_id)
            if entry is None or not entry.get("path"):
                continue
            path = entry["path"]
            crate_name = path[0]
            if crate_name in workspace_crates or crate_name in STANDARD_CRATES:
                continue
            leaked.add("::".join(path))
    return leaked


def configured_allowlist() -> set[str]:
    with ALLOWLIST.open("rb") as handle:
        document = tomllib.load(handle)
    entries = document.get("external_types", [])
    if not isinstance(entries, list) or not all(isinstance(item, str) for item in entries):
        raise ValueError(f"{ALLOWLIST.name}: external_types must be an array of strings")
    if len(entries) != len(set(entries)):
        raise ValueError(f"{ALLOWLIST.name}: external_types contains duplicates")
    return set(entries)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--dump",
        action="store_true",
        help="print the compiler-derived external type set instead of checking it",
    )
    args = parser.parse_args()

    actual = external_types(False) | external_types(True)
    if args.dump:
        for item in sorted(actual):
            print(item)
        return 0

    allowed = configured_allowlist()
    missing = sorted(actual - allowed)
    stale = sorted(allowed - actual)
    if not missing and not stale:
        print(f"facade external types: {len(actual)} allowlisted, no drift")
        return 0

    if missing:
        print("Unallowlisted external types in facade public signatures:", file=sys.stderr)
        for item in missing:
            print(f"  {item}", file=sys.stderr)
    if stale:
        print("Stale facade external-type allowlist entries:", file=sys.stderr)
        for item in stale:
            print(f"  {item}", file=sys.stderr)
    print(
        f"Update {ALLOWLIST.relative_to(api_surface.REPO)} only after reviewing the facade leak.",
        file=sys.stderr,
    )
    return 1


if __name__ == "__main__":
    raise SystemExit(main())
