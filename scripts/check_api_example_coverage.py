#!/usr/bin/env python3
"""Enforce dispositions for Lash's host-facing public API surface.

The public surface comes from rustdoc JSON, not from the inventory being
checked.  This keeps the oracle independent: adding or removing an export
changes the compiler's view and requires an explicit inventory update.
"""

from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import subprocess
import sys
import tomllib
from typing import Any


REPO = Path(__file__).resolve().parents[1]
INVENTORY = REPO / "docs" / "api-example-coverage.toml"
TARGET = Path(os.environ.get("CARGO_TARGET_DIR", REPO / "target"))

AREAS = {
    "sessions-turns",
    "processes",
    "triggers",
    "attachments",
    "stores",
    "plugins",
    "observation-admin",
    "protocol",
}
DISPOSITIONS = {
    "used-asserted",
    "used-unasserted",
    "unused-add",
    "unused-justify",
    "unused-remove",
}
PUBLIC_KINDS = {
    "assoc_const",
    "assoc_type",
    "constant",
    "enum",
    "function",
    "macro",
    "proc_attribute",
    "proc_derive",
    "static",
    "struct",
    "trait",
    "trait_alias",
    "type_alias",
    "union",
}

DEPENDENCY_PACKAGES = {
    "lash_core": ("lash-core", "lash_core", True),
    "lash_http_transport": ("lash-http-transport", "lash_http_transport", True),
    "lash_lashlang_runtime": ("lash-lashlang-runtime", "lash_lashlang_runtime", True),
    "lash_plugin_tool_output_budget": (
        "lash-plugin-tool-output-budget",
        "lash_plugin_tool_output_budget",
        True,
    ),
    "lash_protocol_rlm": ("lash-protocol-rlm", "lash_protocol_rlm", True),
    "lash_remote_protocol": ("lash-remote-protocol", "lash_remote_protocol", True),
    "lash_rlm_types": ("lash-rlm-types", "lash_rlm_types", True),
    "lash_sansio": ("lash-sansio", "lash_sansio", True),
    "lash_tool_support": ("lash-tool-support", "lash_tool_support", True),
    "lash_trace": ("lash-trace", "lash_trace", True),
    "lashlang": ("lashlang", "lashlang", True),
    "schemars": ("schemars", "schemars", False),
    "schemars_derive": ("schemars_derive", "schemars_derive", False),
    "tokio_util": ("tokio-util", "tokio_util", False),
}
_DEPENDENCY_DOCUMENTS: dict[tuple[str, bool], dict[str, Any]] = {}


def item_kind(item: dict[str, Any]) -> str:
    return next(iter(item["inner"]))


def public(visibility: Any) -> bool:
    return visibility == "public"


def target_id(item: dict[str, Any]) -> str | None:
    if item_kind(item) != "use":
        return str(item["id"])
    value = item["inner"]["use"]["id"]
    return None if value is None else str(value)


def exported_name(item: dict[str, Any]) -> str:
    if item.get("name"):
        return item["name"]
    source = item["inner"]["use"]["source"]
    return source.rsplit("::", 1)[-1]


def export_path(prefix: str, item: dict[str, Any]) -> str:
    if item_kind(item) == "use" and item["inner"]["use"]["is_glob"]:
        return prefix
    return f"{prefix}::{exported_name(item)}"


def add_members(
    surface: set[tuple[str, str]],
    path: str,
    item_id: str,
    index: dict[str, dict[str, Any]],
) -> None:
    item = index.get(item_id)
    if item is None:
        return
    kind = item_kind(item)
    inner = item["inner"][kind]

    if kind == "enum":
        for variant_id in inner["variants"]:
            variant = index[str(variant_id)]
            variant_path = f"{path}::{variant['name']}"
            surface.add((variant_path, "variant"))
            variant_shape = variant["inner"]["variant"]["kind"]
            variant_fields: list[int] = []
            if isinstance(variant_shape, dict) and "struct" in variant_shape:
                variant_fields = variant_shape["struct"]["fields"]
            elif isinstance(variant_shape, dict) and "tuple" in variant_shape:
                variant_fields = [
                    field for field in variant_shape["tuple"] if field is not None
                ]
            for field_id in variant_fields:
                field = index[str(field_id)]
                surface.add(
                    (f"{variant_path}::{field.get('name') or field_id}", "field")
                )
    elif kind in {"struct", "union"}:
        shape = inner["kind"] if kind == "struct" else inner
        field_ids: list[int] = []
        if isinstance(shape, dict) and "plain" in shape:
            field_ids = shape["plain"]["fields"]
        elif isinstance(shape, dict) and "tuple" in shape:
            field_ids = [field for field in shape["tuple"] if field is not None]
        elif kind == "union":
            field_ids = inner["fields"]
        for field_id in field_ids:
            field = index[str(field_id)]
            if public(field["visibility"]):
                surface.add((f"{path}::{field.get('name') or field_id}", "field"))

    if kind == "trait":
        for member_id in inner["items"]:
            member = index[str(member_id)]
            surface.add((f"{path}::{member['name']}", item_kind(member)))

    impl_ids = inner.get("impls", []) if isinstance(inner, dict) else []
    for impl_id in impl_ids:
        implementation = index.get(str(impl_id))
        if implementation is None:
            continue
        impl = implementation["inner"].get("impl")
        if impl is None or impl["trait"] is not None:
            continue
        for member_id in impl["items"]:
            member = index.get(str(member_id))
            if member is None or not public(member["visibility"]):
                continue
            surface.add((f"{path}::{member['name']}", item_kind(member)))


def add_export(
    surface: set[tuple[str, str]],
    path: str,
    item: dict[str, Any],
    index: dict[str, dict[str, Any]],
    paths: dict[str, dict[str, Any]],
    all_features: bool,
) -> None:
    item_id = target_id(item)
    target = index.get(item_id) if item_id is not None else None
    member_index = index
    member_paths = paths
    if target is not None:
        kind = item_kind(target)
    elif item_id is not None and str(item_id) in paths:
        # rustdoc does not copy dependency-defined re-export targets into this
        # crate's index.  Their compiler identity and kind remain in `paths`.
        kind = paths[str(item_id)]["kind"]
        resolved = external_target(paths[str(item_id)], all_features)
        if resolved is not None:
            target, dependency = resolved
            member_index = dependency["index"]
            member_paths = dependency["paths"]
            item_id = str(target["id"])
        elif kind in {"enum", "struct", "trait", "union"}:
            canonical = "::".join(paths[str(item_id)]["path"])
            raise RuntimeError(
                f"cannot resolve member-bearing external export {path} ({canonical})"
            )
    else:
        kind = item_kind(item)
    if kind == "module":
        if target is None:
            raise RuntimeError(f"cannot resolve external module export {path}")
        for child_id in target["inner"]["module"]["items"]:
            child = member_index[str(child_id)]
            if not public(child["visibility"]):
                continue
            add_export(
                surface,
                export_path(path, child),
                child,
                member_index,
                member_paths,
                all_features,
            )
        return
    if kind not in PUBLIC_KINDS:
        return
    surface.add((path, kind))
    if target is not None and item_id is not None:
        add_members(surface, path, item_id, member_index)


def lash_surface(document: dict[str, Any], all_features: bool) -> set[tuple[str, str]]:
    index = document["index"]
    paths = document["paths"]
    surface: set[tuple[str, str]] = set()

    def walk(module_id: str, prefix: str) -> None:
        module = index[module_id]["inner"]["module"]
        for child_id in module["items"]:
            child = index[str(child_id)]
            if not public(child["visibility"]):
                continue
            kind = item_kind(child)
            name = exported_name(child)
            path = export_path(prefix, child)
            if kind == "module":
                # Modules organize the inventory but are not callable API
                # entries in their own right.
                walk(str(child_id), path)
            else:
                add_export(surface, path, child, index, paths, all_features)

    walk(str(document["root"]), "lash")
    return surface


def lash_core_surface(document: dict[str, Any], all_features: bool) -> set[tuple[str, str]]:
    """Collect only root host exports, not every item in public internals."""
    index = document["index"]
    paths = document["paths"]
    root = index[str(document["root"])]["inner"]["module"]
    surface: set[tuple[str, str]] = set()
    for child_id in root["items"]:
        child = index[str(child_id)]
        if not public(child["visibility"]) or item_kind(child) == "module":
            continue
        add_export(
            surface,
            export_path("lash_core", child),
            child,
            index,
            paths,
            all_features,
        )
    return surface


def rustdoc(package: str, crate_name: str, all_features: bool) -> dict[str, Any]:
    command = ["cargo", "rustdoc", "-p", package]
    if all_features:
        command.append("--all-features")
    command += ["--lib", "--", "-Z", "unstable-options", "--output-format", "json"]
    env = os.environ.copy()
    env["RUSTC_BOOTSTRAP"] = "1"
    completed = subprocess.run(
        command,
        cwd=REPO,
        env=env,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    if completed.returncode:
        print(completed.stdout, file=sys.stderr, end="")
        completed.check_returncode()
    with (TARGET / "doc" / f"{crate_name}.json").open("rb") as handle:
        return json.load(handle)


def dependency_document(crate_name: str, all_features: bool) -> dict[str, Any] | None:
    package = DEPENDENCY_PACKAGES.get(crate_name)
    if package is None:
        return None
    package_name, rustdoc_name, supports_all_features = package
    configured_all_features = all_features and supports_all_features
    key = (crate_name, configured_all_features)
    if key not in _DEPENDENCY_DOCUMENTS:
        _DEPENDENCY_DOCUMENTS[key] = rustdoc(
            package_name, rustdoc_name, configured_all_features
        )
    return _DEPENDENCY_DOCUMENTS[key]


def external_target(
    path_entry: dict[str, Any], all_features: bool
) -> tuple[dict[str, Any], dict[str, Any]] | None:
    canonical_path = path_entry["path"]
    if not canonical_path:
        return None
    document = dependency_document(canonical_path[0], all_features)
    if document is None:
        return None
    for item_id, candidate in document["paths"].items():
        if candidate["path"] != canonical_path or candidate["kind"] != path_entry["kind"]:
            continue
        item = document["index"].get(str(item_id))
        if item is not None:
            return item, document
    return None


def configured_surface(all_features: bool) -> set[tuple[str, str]]:
    surface = lash_surface(
        rustdoc("lash-runtime", "lash", all_features), all_features
    )
    core = lash_core_surface(
        rustdoc("lash-core", "lash_core", all_features), all_features
    )
    overlap = surface & core
    if overlap:
        raise AssertionError(f"crate-qualified API names overlap: {sorted(overlap)}")
    surface.update(core)
    return surface


def current_surface() -> list[tuple[str, str, str]]:
    default = configured_surface(False)
    all_features = configured_surface(True)
    surface = default | all_features
    return [
        (
            symbol,
            kind,
            "default+all-features"
            if (symbol, kind) in default and (symbol, kind) in all_features
            else "default"
            if (symbol, kind) in default
            else "all-features",
        )
        for symbol, kind in sorted(surface)
    ]


def relocated_reference(reference: str) -> str | None:
    """Re-anchor a stale reference by exact source text within the same file.

    Returns the corrected reference, or None when the file is gone or no line
    matches the recorded source text — those need a human, not a refresh."""
    try:
        location, needle = reference.split("#", 1)
        relative, _ = location.rsplit(":", 1)
    except (ValueError, TypeError):
        return None
    path = REPO / relative
    if not path.is_relative_to(REPO / "examples") or not path.is_file():
        return None
    lines = path.read_text(encoding="utf-8").splitlines()
    hits = [index + 1 for index, line in enumerate(lines) if needle in line]
    if not hits:
        return None
    return f"{relative}:{hits[0]}#{needle}"


def refresh() -> int:
    """Re-anchor every stale evidence reference whose source text still exists.

    Line numbers move whenever an example file is edited above an anchor; the
    recorded source text is the durable identity. Anything whose text is gone is
    reported and left untouched — deleting or rewriting evidence is a reviewed
    decision, not a refresh."""
    text = INVENTORY.read_text(encoding="utf-8")
    stale: list[str] = []
    replaced = 0
    with INVENTORY.open("rb") as handle:
        inventory = tomllib.load(handle)
    for entry in inventory.get("api", []):
        for field in ("usage", "assertion"):
            reference = entry.get(field, "")
            if not reference or reference_exists(reference):
                continue
            corrected = relocated_reference(reference)
            if corrected is None:
                stale.append(f"{entry.get('symbol')}: {field} evidence gone: {reference!r}")
                continue
            escaped_old = toml_escape(reference)
            escaped_new = toml_escape(corrected)
            if escaped_old in text:
                text = text.replace(escaped_old, escaped_new)
                replaced += 1
            else:
                stale.append(f"{entry.get('symbol')}: could not rewrite {field} reference")
    INVENTORY.write_text(text, encoding="utf-8")
    print(f"refreshed {replaced} evidence anchors")
    for line in stale:
        print(f"- {line}", file=sys.stderr)
    return 1 if stale else 0


def toml_escape(value: str) -> str:
    return value.replace("\\", "\\\\").replace('"', '\\"')


def reference_exists(reference: str) -> bool:
    try:
        location, needle = reference.split("#", 1)
        relative, line_text = location.rsplit(":", 1)
        line_number = int(line_text)
    except (ValueError, TypeError):
        return False
    path = REPO / relative
    examples = REPO / "examples"
    if not path.is_relative_to(examples) or not path.is_file() or line_number < 1:
        return False
    lines = path.read_text(encoding="utf-8").splitlines()
    return line_number <= len(lines) and needle in lines[line_number - 1]


def check() -> int:
    with INVENTORY.open("rb") as handle:
        document = tomllib.load(handle)
    entries = document.get("api", [])
    by_api: dict[tuple[str, str], dict[str, Any]] = {}
    errors: list[str] = []
    for entry in entries:
        symbol = entry.get("symbol", "")
        kind = entry.get("kind", "")
        availability = entry.get("availability", "")
        api = (symbol, kind)
        if api in by_api:
            errors.append(f"duplicate inventory entry: {symbol} ({kind})")
        by_api[api] = entry
        if availability not in {"default", "all-features", "default+all-features"}:
            errors.append(f"{symbol}: invalid availability {availability!r}")
        if entry.get("area") not in AREAS:
            errors.append(f"{symbol}: unknown area {entry.get('area')!r}")
        disposition = entry.get("disposition")
        if disposition not in DISPOSITIONS:
            errors.append(f"{symbol}: unknown disposition {disposition!r}")
        usage = entry.get("usage", "")
        assertion = entry.get("assertion", "")
        if disposition in {"used-asserted", "used-unasserted"}:
            if not reference_exists(usage):
                errors.append(f"{symbol}: stale or invalid example usage reference {usage!r}")
        if disposition == "used-asserted":
            if not reference_exists(assertion):
                errors.append(
                    f"{symbol}: stale or invalid example assertion reference {assertion!r}"
                )
        elif assertion:
            errors.append(f"{symbol}: only used-asserted entries may name an assertion")
        if disposition.startswith("unused-") and not entry.get("reason", "").strip():
            errors.append(f"{symbol}: unused disposition requires a concrete reason")
        if disposition == "unused-remove" and "Breaking:" not in entry.get("reason", ""):
            errors.append(f"{symbol}: removal disposition requires a Breaking: note")

    actual = current_surface()
    recorded = set(by_api)
    actual_by_api = {
        (symbol, kind): availability for symbol, kind, availability in actual
    }
    compiler_api = set(actual_by_api)
    for symbol, kind in sorted(compiler_api - recorded):
        errors.append(f"undispositioned public API: {symbol} ({kind})")
    for symbol, kind in sorted(recorded - compiler_api):
        errors.append(f"inventory names an API that is no longer public: {symbol} ({kind})")
    for api in sorted(recorded & compiler_api):
        recorded_availability = by_api[api].get("availability")
        if recorded_availability != actual_by_api[api]:
            errors.append(
                f"{api[0]} ({api[1]}): availability changed from "
                f"{recorded_availability!r} to {actual_by_api[api]!r}"
            )

    if errors:
        print("API example-coverage contract failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print(f"API example-coverage contract passed ({len(actual)} entries)")
    return 0


def dump_surface() -> int:
    surface = current_surface()
    json.dump(
        [
            {"symbol": symbol, "kind": kind, "availability": availability}
            for symbol, kind, availability in surface
        ],
        sys.stdout,
        indent=2,
    )
    print()
    return 0


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument(
        "--dump-surface",
        action="store_true",
        help="print the compiler-derived surface as JSON instead of checking",
    )
    parser.add_argument(
        "--refresh",
        action="store_true",
        help="re-anchor stale evidence references whose source text still exists, then exit",
    )
    args = parser.parse_args()
    if args.dump_surface:
        return dump_surface()
    if args.refresh:
        return refresh()
    return check()


if __name__ == "__main__":
    raise SystemExit(main())
