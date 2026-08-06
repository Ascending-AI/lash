#!/usr/bin/env python3
r"""Enforce dispositions for Lash's host-facing public API surface.

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

The unit of *disposition* is the **API item**, not the path.  A single item is
usually visible under several paths -- `lash::Session` and `lash_core::Session`
are one struct, and a reachability-closure type is often also a facade export --
so every path is keyed by the compiler's canonical identity for what it names,
and paths that share an identity are one inventory row.  That row lives at the
item's *primary* path (the facade path a host would write, when one exists).
Two rows for one item are therefore a contract contradiction, not a duplicate:
see `api_items` and the contradiction error in `item_errors`.

The unit of *existence* is still the **path**.  An alias carries no verdict, but
it is a promise: ADR 0051 makes retiring a `lash_core::` re-export breaking for
direct core consumers, wave by wave.  So each row records its item's remaining
paths in `aliases`, exactly as `availability` and `kind` are recorded -- derived
from the compiler, stored so that a change has to be acknowledged rather than
noticed.  Adding, removing, or moving any path fails the gate; only the
*disposition* is centralized, never the path set.  `--dump-surface` prints the
same projection the check compares against.

Justification prose carries three lints, because the prose is the evidence:

* No machine-local paths.  `/workspace/...`, `~/...`, and `C:\...` are one
  developer's checkout, not a contract another reader can verify.  Evidence
  must be repository-relative; `./`-prefixed paths are fine.
* No impossible facade migrations.  A crate the `lash` facade is built on
  cannot depend on the facade, so a justification may not park a consumer in
  such a crate behind a pending migration to it.  ADR 0051 names this cycle for
  `lash-remote-protocol`; the rule holds for every crate the facade is built on,
  and the set comes from `cargo metadata`, not a hand-written list.  Detection
  is per sentence and negation-aware: stating that a caller *cannot* migrate is
  the honest description of the same fact, and the cited crate must be named in
  the sentence making the claim so the error blames the right path.
* No tautological assertion anchors.  `size_of::<T>() > 0` holds for every
  non-ZST, so an anchor on one proves the path resolves and nothing else.  An
  item whose only evidence is a tautology is undispositioned, not exercised.

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
import re
import subprocess
import sys
import tomllib
from typing import Any, NamedTuple


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
    surface: dict[tuple[str, str], str],
    path: str,
    canonical: str,
    item_id: str,
    index: dict[str, dict[str, Any]],
) -> None:
    """Record every public member of `path` under both its path and its identity.

    `canonical` is the owner's compiler identity, so a member's identity is that
    identity plus the member name.  The same method reached through the facade
    and through `lash_core` therefore lands on one identity.
    """
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
            variant_canonical = f"{canonical}::{variant['name']}"
            surface[(variant_path, "variant")] = variant_canonical
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
                name = field.get("name") or field_id
                surface[(f"{variant_path}::{name}", "field")] = (
                    f"{variant_canonical}::{name}"
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
                name = field.get("name") or field_id
                surface[(f"{path}::{name}", "field")] = f"{canonical}::{name}"

    if kind == "trait":
        for member_id in inner["items"]:
            member = index[str(member_id)]
            if doc_hidden(member):
                continue
            name = member["name"]
            surface[(f"{path}::{name}", item_kind(member))] = f"{canonical}::{name}"

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
            name = member["name"]
            surface[(f"{path}::{name}", item_kind(member))] = f"{canonical}::{name}"


def canonical_identity(
    item_id: str | None, paths: dict[str, dict[str, Any]], fallback: str
) -> str:
    """The compiler's canonical path for an item, or `fallback` when it has none.

    This is the item's identity across every path that reaches it.  Rustdoc's
    `paths` table records it for local and dependency-defined items alike, which
    is what lets a facade re-export and its `lash_core` original collapse onto
    one row.  Unresolved re-exports (globs) have no identity of their own, so the
    export path stands in for one.
    """
    entry = paths.get(str(item_id)) if item_id is not None else None
    if entry is None or not entry.get("path"):
        return fallback
    return "::".join(entry["path"])


def add_export(
    surface: dict[tuple[str, str], str],
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
    canonical = canonical_identity(item_id, member_paths, path)
    surface[(path, kind)] = canonical
    if target is not None and item_id is not None:
        add_members(surface, path, canonical, item_id, member_index)


def lash_surface(
    document: dict[str, Any], all_features: bool
) -> dict[tuple[str, str], str]:
    index = document["index"]
    paths = document["paths"]
    surface: dict[tuple[str, str], str] = {}

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


def lash_core_surface(
    document: dict[str, Any], all_features: bool
) -> dict[tuple[str, str], str]:
    """Root host exports plus every core-owned type they make reachable.

    Root exports are the named surface; the reachability closure is the rest of
    the contract. Nested public modules are not walked -- an item there enters
    the gate only when a publicly reachable signature exposes it, which is the
    same thing as being host-visible.
    """
    index = document["index"]
    paths = document["paths"]
    root = index[str(document["root"])]["inner"]["module"]
    surface: dict[tuple[str, str], str] = {}
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
        # A closure type is keyed by its canonical path, so path *is* identity.
        surface[(path, item_kind(index[item_id]))] = path
        add_members(surface, path, path, item_id, index)
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


def configured_surface(all_features: bool) -> dict[tuple[str, str], str]:
    surface = lash_surface(
        rustdoc("lash-runtime", "lash", all_features), all_features
    )
    core = lash_core_surface(
        rustdoc("lash-core", "lash_core", all_features), all_features
    )
    overlap = set(surface) & set(core)
    if overlap:
        raise AssertionError(f"crate-qualified API names overlap: {sorted(overlap)}")
    surface.update(core)
    return surface


def primary_path(aliases: list[str]) -> str:
    """The one path an item's inventory row is recorded at.

    Prefer what a host writes: the `lash` facade path, then the shortest path,
    then lexicographic order.  The choice only has to be deterministic and
    stable; picking the facade keeps the inventory readable as host contract
    rather than as core internals.
    """
    return min(
        aliases,
        key=lambda alias: (
            0 if alias.startswith("lash::") else 1,
            alias.count("::"),
            alias,
        ),
    )


def api_items(surface: dict[tuple[str, str], str]) -> dict[tuple[str, str], list[str]]:
    """Group surface paths into API items keyed by `(identity, kind)`.

    Values are every path that reaches the item, sorted; the first element is
    not special -- use `primary_path`.
    """
    grouped: dict[tuple[str, str], list[str]] = {}
    for (path, kind), identity in surface.items():
        grouped.setdefault((identity, kind), []).append(path)
    return {key: sorted(paths) for key, paths in grouped.items()}


class ApiItem(NamedTuple):
    """One unit of contract: the thing a disposition is about."""

    #: The path the inventory row lives at.
    primary: str
    kind: str
    availability: str
    #: Every path that reaches this item, primary included.
    paths: list[str]
    #: The compiler's canonical path -- the identity that made these one item.
    identity: str

    def aliases(self) -> list[str]:
        return [path for path in self.paths if path != self.primary]


def current_surface() -> list[ApiItem]:
    """One record per API item, sorted by primary path.

    Availability is the item's, not a path's: an item is available in a
    configuration when any path reaches it there.
    """
    default = configured_surface(False)
    all_features = configured_surface(True)
    surface = {**all_features, **default}
    conflicts = sorted(
        key for key in set(default) & set(all_features) if default[key] != all_features[key]
    )
    if conflicts:
        raise AssertionError(f"API identity differs between configurations: {conflicts}")

    items: list[ApiItem] = []
    for (identity, kind), paths in api_items(surface).items():
        in_default = any((path, kind) in default for path in paths)
        in_all_features = any((path, kind) in all_features for path in paths)
        availability = (
            "default+all-features"
            if in_default and in_all_features
            else "default"
            if in_default
            else "all-features"
        )
        items.append(
            ApiItem(primary_path(paths), kind, availability, paths, identity)
        )
    return sorted(items)


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


#: A token that names somewhere on one machine rather than in this repository.
#: `./x/y` is repository-relative, so a leading `.` must survive tokenizing.
MACHINE_LOCAL_ROOT = re.compile(r"^(?:/|~/|\\\\|[A-Za-z]:[\\/]|file://)")
#: Any wording that hands a consumer to the facade: migrate/move/port/switch to it.
MIGRATION_TO_FACADE = re.compile(
    r"\b(?:migrat|mov|port|switch|transition|relocat)\w*\b[^.]{0,60}?\bto\b[^.]{0,40}?"
    r"\b(?:fa[cç]ade|lash)\b",
    re.IGNORECASE,
)
#: Wording that denies the migration instead of promising it.
MIGRATION_DENIED = re.compile(
    r"\b(?:cannot|can ?not|can't|never|unable|impossible|no longer|not)\b", re.IGNORECASE
)
#: Assertions that hold for every non-ZST and therefore assert nothing.
TAUTOLOGICAL_ASSERTION = re.compile(r"\b(?:size_of|align_of)\b")


def path_tokens(text: str) -> list[str]:
    """Path-shaped words in prose, stripped of quoting and sentence punctuation.

    A trailing `.` is sentence punctuation, but a leading one is part of a
    `./`-relative path, so the two ends strip differently.
    """
    tokens = []
    for word in re.split(r"[\s,;]+", text):
        token = word.lstrip("`'\"([{<“‘").rstrip("`'\"）)]}>”’.:,;")
        if token:
            tokens.append(token)
    return tokens


def machine_local_path(text: str) -> str | None:
    """The first absolute or home-relative path in `text`, if any.

    Absolute evidence is unverifiable for everyone but its author: the path
    describes one checkout on one machine, and nothing in review or CI can tell
    whether it still says what it claimed.  Repository-relative evidence is
    checkable by anyone holding the repository.
    """
    for token in path_tokens(text):
        if not MACHINE_LOCAL_ROOT.match(token):
            continue
        if "/" in token[1:] or "\\" in token[1:]:
            return token
    return None


def tautological_assertion(assertion: str) -> bool:
    """Whether an assertion anchor asserts something that is always true.

    `assert!(size_of::<T>() > 0)` holds for every non-ZST, so it witnesses that
    the path compiles and nothing about whether a host exercised the item.  An
    item resting on one is undispositioned, not covered.
    """
    return bool(TAUTOLOGICAL_ASSERTION.search(assertion.split("#", 1)[-1]))


def facade_dependency_dirs() -> set[str]:
    """Workspace crate directories the `lash` facade is built on.

    A crate in this set cannot depend on the facade -- that would be a cycle --
    so a consumer living there can never "migrate to the lash facade".  Derived
    from the resolved dependency graph so the rule cannot drift out of date the
    way a hand-written crate list would.
    """
    completed = subprocess.run(
        # All features, so a feature-gated dependency edge cannot hide a cycle.
        ["cargo", "metadata", "--format-version", "1", "--all-features", "--locked"],
        cwd=REPO,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
    )
    metadata = json.loads(completed.stdout)
    members = set(metadata["workspace_members"])
    packages = {package["id"]: package for package in metadata["packages"]}
    dependencies = {
        node["id"]: [dependency["pkg"] for dependency in node["deps"]]
        for node in metadata["resolve"]["nodes"]
    }
    facade = next(
        package_id
        for package_id, package in packages.items()
        if package["name"] == "lash-runtime"
    )
    reached: set[str] = set()
    frontier = [facade]
    while frontier:
        current = frontier.pop()
        for dependency in dependencies.get(current, []):
            if dependency in reached:
                continue
            reached.add(dependency)
            frontier.append(dependency)
    directories = set()
    for package_id in reached & members:
        manifest = Path(packages[package_id]["manifest_path"]).parent
        if manifest.is_relative_to(REPO):
            directories.add(manifest.relative_to(REPO).as_posix())
    return directories


def impossible_facade_migration(reason: str, facade_dirs: set[str]) -> str | None:
    """The cited consumer path that cannot migrate to the facade, if any.

    `lash-core` is the loud case: it defines the types the facade re-exports, so
    a justification saying a `crates/lash-core/...` caller will move to `lash`
    describes a dependency cycle.  The seam is real; only the migration story is
    fiction -- which is why the check is on the promise, not on the phrase.

    Two properties keep it honest.  It reads sentence by sentence, so the crate
    blamed is the one the claim is about rather than any path mentioned anywhere
    in the prose.  And a denial is not a claim: "that caller *cannot* migrate to
    the `lash` facade" states the very cycle this rejects, so rejecting it would
    punish the correct description.
    """
    for sentence in re.split(r"(?<=[.])\s+|\n", reason):
        claim = MIGRATION_TO_FACADE.search(sentence)
        if claim is None:
            continue
        if MIGRATION_DENIED.search(sentence[: claim.end()]):
            continue
        for token in path_tokens(sentence):
            location = token.rsplit(":", 1)[0] if ":" in token else token
            for directory in facade_dirs:
                if location == directory or location.startswith(f"{directory}/"):
                    return token
    return None


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


def item_errors(
    by_api: dict[tuple[str, str], dict[str, Any]], items: list[ApiItem]
) -> list[str]:
    """Reconcile the inventory against the compiler's API items.

    One item is one row.  Rows found under several of an item's paths are the
    failure this exists to catch: the same function was `unused-add` through the
    facade and `unused-remove` through `lash_core`, which is two answers to one
    contract question.  The contradiction is named explicitly because its fix is
    a decision, while a mere repeat of the same disposition just needs the alias
    row dropped.

    Centralizing the *verdict* must not decentralize *existence*: the row's
    `aliases` are compared against the compiler's projection, so retiring or
    adding a `lash_core::` re-export -- breaking for direct core consumers under
    ADR 0051 -- still fails the gate even though it changes no disposition.
    """
    errors: list[str] = []
    matched: set[tuple[str, str]] = set()
    for item in items:
        primary, kind = item.primary, item.kind
        rows = [(path, by_api[(path, kind)]) for path in item.paths if (path, kind) in by_api]
        matched.update((path, kind) for path, _ in rows)
        if not rows:
            errors.append(f"undispositioned public API: {primary} ({kind})")
            continue
        if len(rows) > 1:
            detail = ", ".join(
                f"{alias} = {row.get('disposition')!r}" for alias, row in rows
            )
            if len({row.get("disposition") for _, row in rows}) > 1:
                errors.append(
                    f"{primary} ({kind}): contradictory dispositions for one API "
                    f"item: {detail}. One item carries one disposition; keep the "
                    f"row at {primary} and delete the others."
                )
            else:
                errors.append(
                    f"{primary} ({kind}): one API item recorded under several of "
                    f"its paths: {detail}. An alias belongs in this item's row at "
                    f"{primary} as an alias, not as a second row."
                )
            continue
        recorded_path, row = rows[0]
        if recorded_path != primary:
            errors.append(
                f"{primary} ({kind}): recorded at the alias path {recorded_path}; "
                f"record each API item at its primary path"
            )
        if row.get("availability") != item.availability:
            errors.append(
                f"{primary} ({kind}): availability changed from "
                f"{row.get('availability')!r} to {item.availability!r}"
            )
        recorded_aliases = row.get("aliases") or []
        if sorted(recorded_aliases) != item.aliases():
            retired = sorted(set(recorded_aliases) - set(item.aliases()))
            added = sorted(set(item.aliases()) - set(recorded_aliases))
            detail = []
            if retired:
                detail.append(
                    f"no longer public: {', '.join(retired)} -- retiring a path is "
                    f"breaking for its direct consumers (ADR 0051)"
                )
            if added:
                detail.append(
                    f"newly public: {', '.join(added)} -- a new path is a new promise"
                )
            errors.append(
                f"{primary} ({kind}): the item's public paths changed. "
                f"{'; '.join(detail)}. Record the change in this row's aliases."
            )
    for symbol, kind in sorted(set(by_api) - matched):
        errors.append(f"inventory names an API that is no longer public: {symbol} ({kind})")
    return errors


def check() -> int:
    with INVENTORY.open("rb") as handle:
        document = tomllib.load(handle)
    entries = document.get("api", [])
    by_api: dict[tuple[str, str], dict[str, Any]] = {}
    errors: list[str] = []
    facade_dirs = facade_dependency_dirs()
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
        aliases = entry.get("aliases", [])
        if not isinstance(aliases, list) or not all(
            isinstance(alias, str) for alias in aliases
        ):
            errors.append(f"{symbol}: aliases must be a list of paths")
        elif aliases != sorted(aliases) or symbol in aliases or len(set(aliases)) != len(aliases):
            errors.append(
                f"{symbol}: aliases must be sorted, distinct, and exclude the primary path"
            )
        elif "aliases" in entry and not aliases:
            errors.append(f"{symbol}: omit aliases rather than recording an empty list")
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
            elif tautological_assertion(assertion):
                errors.append(
                    f"{symbol}: assertion anchor {assertion!r} is a tautology -- "
                    "size_of/align_of holds for every non-ZST and proves only that "
                    "the path resolves. Assert an outcome the runtime produced."
                )
        elif assertion:
            errors.append(f"{symbol}: only used-asserted entries may name an assertion")
        if disposition.startswith("unused-") and not entry.get("reason", "").strip():
            errors.append(f"{symbol}: unused disposition requires a concrete reason")
        if disposition == "unused-remove" and "Breaking:" not in entry.get("reason", ""):
            errors.append(f"{symbol}: removal disposition requires a Breaking: note")
        reason = entry.get("reason", "")
        for field, value in (("reason", reason), ("usage", usage), ("assertion", assertion)):
            # For evidence references only the location is a path; the anchored
            # source text after `#` is code, and code may legitimately say `/`.
            offender = machine_local_path(
                value if field == "reason" else value.split("#", 1)[0]
            )
            if offender:
                errors.append(
                    f"{symbol}: {field} names the machine-local path {offender!r}; "
                    "evidence must be repository-relative"
                )
        migration = impossible_facade_migration(reason, facade_dirs)
        if migration:
            errors.append(
                f"{symbol}: reason parks the consumer at {migration!r} behind a "
                "migration to the lash facade, but the facade is built on that "
                "crate and it cannot depend on the facade"
            )

    items = current_surface()
    errors += item_errors(by_api, items)

    if errors:
        print("API example-coverage contract failed:", file=sys.stderr)
        for error in errors:
            print(f"- {error}", file=sys.stderr)
        return 1
    print(f"API example-coverage contract passed ({len(items)} entries)")
    return 0


def dump_surface() -> int:
    json.dump(
        [
            {
                "symbol": item.primary,
                "kind": item.kind,
                "availability": item.availability,
                "identity": item.identity,
                "aliases": item.aliases(),
            }
            for item in current_surface()
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
