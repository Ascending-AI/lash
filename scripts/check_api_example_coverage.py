#!/usr/bin/env python3
"""Enforce dispositions for Lash's host-facing public API surface.

The public surface comes from rustdoc JSON, not from the inventory being
checked.  This keeps the oracle independent: adding or removing an export
changes the compiler's view and requires an explicit inventory update.

`lash-runtime` is walked through its public module tree; every path a host can
write is a named export.  `lash-core` cannot be walked that way -- its nested
public modules are runtime internals that happen to be reachable -- so its
surface is the root exports plus their *reachability closure*: every
core-owned type a root export hands to a host through a public signature.
Those types are keyed by the compiler's canonical path, which is often rooted
in a `pub(crate)` module and therefore appears nowhere in a root walk, even
though every public method on the value is callable.  See
`core_reachable_types`.

Three exclusions are deliberate, and each is enforced structurally rather than
by a hand-maintained list:

1. `#[doc(hidden)]` items.  Hidden means "not host contract"; rustdoc omits
   them from this JSON unless `--document-hidden-items` is passed, which
   `rustdoc()` never does, and `doc_hidden` refuses them a second time so a
   future rustdoc that stops omitting them cannot silently widen the gate.
   `lash_core::runtime::await_event_coordinator` is the current example: a
   doc-hidden re-export of a `pub(crate)`-rooted module, deliberately outside
   the gate.
2. Items in nested `lash-core` public modules that no publicly reachable
   signature mentions.  They are internals; if one becomes host-visible it
   enters through the reachability closure automatically.
3. Items owned by another crate and not re-exported by `lash` or `lash-core`.
   Each crate answers for its own surface.
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


#: Type-bearing rustdoc kinds a reachability edge can land on.
REACHABLE_KINDS = {"enum", "struct", "trait", "type_alias", "union"}


def item_kind(item: dict[str, Any]) -> str:
    return next(iter(item["inner"]))


def public(visibility: Any) -> bool:
    return visibility == "public"


def doc_hidden(item: dict[str, Any]) -> bool:
    """Whether rustdoc recorded `#[doc(hidden)]` on this item.

    Hidden items are not host contract, so they stay outside the gate. Today
    rustdoc already omits them from the JSON; this keeps the exclusion true by
    intent rather than by side effect.
    """
    for attribute in item.get("attrs") or []:
        if isinstance(attribute, str):
            if "doc(hidden)" in attribute.replace(" ", ""):
                return True
        elif isinstance(attribute, dict) and "doc_hidden" in attribute:
            return True
    return False


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
            if doc_hidden(variant):
                continue
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
                if doc_hidden(field):
                    continue
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
            if public(field["visibility"]) and not doc_hidden(field):
                surface.add((f"{path}::{field.get('name') or field_id}", "field"))

    if kind == "trait":
        for member_id in inner["items"]:
            member = index[str(member_id)]
            if doc_hidden(member):
                continue
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
            if doc_hidden(member):
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
            if not public(child["visibility"]) or doc_hidden(child):
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
            if not public(child["visibility"]) or doc_hidden(child):
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


def referenced_ids(node: Any, found: set[str]) -> None:
    """Collect every resolved item id a rustdoc type/generics node names."""
    if isinstance(node, list):
        for element in node:
            referenced_ids(element, found)
        return
    if not isinstance(node, dict):
        return
    for key, value in node.items():
        if key == "resolved_path":
            if value.get("id") is not None:
                found.add(str(value["id"]))
            referenced_ids(value.get("args"), found)
        elif key in {"primitive", "generic", "infer"}:
            continue
        else:
            referenced_ids(value, found)


def signature_ids(member: dict[str, Any]) -> set[str]:
    """Item ids a public member exposes through its signature."""
    found: set[str] = set()
    kind = item_kind(member)
    inner = member["inner"][kind]
    if kind == "function":
        referenced_ids(inner.get("sig"), found)
        referenced_ids(inner.get("generics"), found)
    elif kind in {"assoc_const", "assoc_type", "constant", "static"}:
        referenced_ids(inner.get("type"), found)
        referenced_ids(inner.get("bounds"), found)
    return found


def exposed_ids(item_id: str, index: dict[str, dict[str, Any]]) -> set[str]:
    """Item ids an export hands to a host: field, variant, and member types.

    Standalone exports carry their exposure in their own signature; types carry
    it in their public fields, variants, and inherent or trait members.
    """
    item = index.get(item_id)
    if item is None:
        return set()
    kind = item_kind(item)
    if kind not in REACHABLE_KINDS:
        return signature_ids(item)
    inner = item["inner"][kind]
    found: set[str] = set()

    if kind == "type_alias":
        referenced_ids(inner.get("type"), found)
        return found

    field_ids: list[Any] = []
    if kind == "enum":
        for variant_id in inner["variants"]:
            variant = index.get(str(variant_id))
            if variant is None or doc_hidden(variant):
                continue
            shape = variant["inner"]["variant"]["kind"]
            if isinstance(shape, dict) and "struct" in shape:
                field_ids += shape["struct"]["fields"]
            elif isinstance(shape, dict) and "tuple" in shape:
                field_ids += [field for field in shape["tuple"] if field is not None]
    elif kind in {"struct", "union"}:
        shape = inner["kind"] if kind == "struct" else inner
        if isinstance(shape, dict) and "plain" in shape:
            field_ids = shape["plain"]["fields"]
        elif isinstance(shape, dict) and "tuple" in shape:
            field_ids = [field for field in shape["tuple"] if field is not None]
        elif kind == "union":
            field_ids = inner["fields"]
        field_ids = [
            field_id
            for field_id in field_ids
            if public((index.get(str(field_id)) or {}).get("visibility"))
        ]
    for field_id in field_ids:
        field = index.get(str(field_id))
        if field is None or doc_hidden(field):
            continue
        referenced_ids(field["inner"]["struct_field"], found)

    members: list[dict[str, Any]] = []
    if kind == "trait":
        members += [
            member
            for member_id in inner["items"]
            if (member := index.get(str(member_id))) is not None
        ]
    for impl_id in inner.get("impls", []) if isinstance(inner, dict) else []:
        implementation = index.get(str(impl_id))
        if implementation is None:
            continue
        impl = implementation["inner"].get("impl")
        # Inherent impls only: a trait impl's members belong to the trait.
        if impl is None or impl["trait"] is not None:
            continue
        members += [
            member
            for member_id in impl["items"]
            if (member := index.get(str(member_id))) is not None
            and public(member["visibility"])
        ]
    for member in members:
        if doc_hidden(member):
            continue
        found |= signature_ids(member)
    return found


def core_reachable_types(
    seeds: set[str], index: dict[str, dict[str, Any]], paths: dict[str, dict[str, Any]]
) -> dict[str, str]:
    """Core-owned types a host can reach from the root exports, but cannot name.

    A root export's public method can return, or accept, a type whose only
    module path is `pub(crate)`. The value is then in host hands and every
    public method on it is callable, yet a root walk never mentions it. This
    closes over public signatures, fields, and variants from the root exports
    until no new core-owned type appears, and keys each result by the
    compiler's canonical path -- the only stable identity such a type has.

    Returns `{canonical path: item id}` for types outside `seeds`.
    """
    reached = set(seeds)
    frontier = list(seeds)
    while frontier:
        for candidate in exposed_ids(frontier.pop(), index):
            if candidate in reached:
                continue
            item = index.get(candidate)
            # Absent from this index means another crate owns it.
            if item is None or item_kind(item) not in REACHABLE_KINDS:
                continue
            if doc_hidden(item):
                continue
            reached.add(candidate)
            frontier.append(candidate)

    named: dict[str, str] = {}
    for item_id in sorted(reached - seeds):
        entry = paths.get(item_id)
        if entry is None:
            raise RuntimeError(f"reachable core type {item_id} has no canonical path")
        named["::".join(entry["path"])] = item_id
    return named


def lash_core_surface(document: dict[str, Any], all_features: bool) -> set[tuple[str, str]]:
    """Root host exports plus every core-owned type they make reachable.

    Root exports are the named surface; the reachability closure is the rest of
    the contract. Nested public modules are not walked -- an item there enters
    the gate only when a publicly reachable signature exposes it, which is the
    same thing as being host-visible.
    """
    index = document["index"]
    paths = document["paths"]
    root = index[str(document["root"])]["inner"]["module"]
    surface: set[tuple[str, str]] = set()
    seeds: set[str] = set()
    for child_id in root["items"]:
        child = index[str(child_id)]
        if not public(child["visibility"]) or item_kind(child) == "module":
            continue
        if doc_hidden(child):
            continue
        add_export(
            surface,
            export_path("lash_core", child),
            child,
            index,
            paths,
            all_features,
        )
        target = target_id(child)
        if target is not None and target in index:
            seeds.add(target)

    for path, item_id in core_reachable_types(seeds, index, paths).items():
        surface.add((path, item_kind(index[item_id])))
        add_members(surface, path, item_id, index)
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
