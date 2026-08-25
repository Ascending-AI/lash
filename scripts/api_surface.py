#!/usr/bin/env python3
"""Discover and snapshot Lash's compiler-derived facade API surface.

The discovery is shared with check_api_example_coverage.py so the checked
snapshot and the disposition ledger always read the same rustdoc graph,
canonical identities, aliases, and feature availability.
"""

from __future__ import annotations

import argparse
import difflib
import json
import os
from pathlib import Path
import sys
import tomllib
from typing import Any, NamedTuple


SCRIPTS = Path(__file__).resolve().parent
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

import rustdoc_json_cache  # noqa: E402  (sibling script, located above)

REPO = Path(__file__).resolve().parents[1]
INVENTORY = REPO / "docs" / "api-example-coverage.toml"
SNAPSHOT = REPO / "docs" / "api-surface.snapshot"

def target_directory(environment: dict[str, str] | None = None) -> Path:
    """Cargo's target directory for this checkout, as an absolute path.

    A relative `CARGO_TARGET_DIR` is resolved by cargo against its *working
    directory*, and `rustdoc()` runs cargo at the repository root, so the
    repository is where a relative value lands.  This script may be invoked
    from anywhere, so it cannot resolve one against its own cwd and still name
    the directory cargo wrote to.  The invariant belongs to that call site, not
    to cargo: generation moving to another cwd would have to move this with it.
    """
    if environment is None:
        environment = dict(os.environ)
    return REPO / Path(environment.get("CARGO_TARGET_DIR") or "target")


TARGET = target_directory()
#: Where this gate's rustdoc builds live.
#:
#: Every `cargo doc` and `cargo rustdoc` in a checkout writes
#: `<target>/doc/<crate>.json`, so a document at that path belongs to whichever
#: documentation build ran last rather than to the invocation that is about to
#: read it.  The rustdoc gate documents these same two crates with
#: `--all-features`; a run of it that lands between this gate's default-features
#: rustdoc and this gate's read hands the default pass an all-features document,
#: which is an availability flip on every `testing`-gated item -- and, because
#: the rustdoc-JSON cache stores what it finds at the destination, one that
#: outlives the run that produced it.  A subdirectory of its own makes the
#: document this gate reads a function of the command this gate issued.
#:
#: Derived from `CARGO_TARGET_DIR` rather than named absolutely so a worktree
#: keeps its artifacts inside its own target directory, and so the isolation
#: survives `orb`-style forks that relocate it.
GATE_TARGET = TARGET / "coverage-gate"

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

#: Crates whose rustdoc JSON this check may need in order to resolve a
#: re-exported member-bearing type.  The boolean is "first party": a workspace
#: crate answers to this repository's feature graph and to its ledger, so it is
#: documented with `--all-features` and with hidden items; a third-party crate
#: answers to neither, and its hidden internals are not this inventory's
#: business.  A spec is version-qualified when the graph holds two majors.
DEPENDENCY_PACKAGES = {
    "lash_core": ("lash-internal-core", "lash_core", True),
    "lash_http_transport": ("lash-internal-http-transport", "lash_http_transport", True),
    "lash_lashlang_runtime": ("lash-internal-lashlang-runtime", "lash_lashlang_runtime", True),
    "lash_typescript": ("lash-internal-typescript", "lash_typescript", True),
    "lash_plugin_tool_output_budget": (
        "lash-internal-plugin-tool-output-budget",
        "lash_plugin_tool_output_budget",
        True,
    ),
    "lash_protocol_rlm": ("lash-internal-protocol-rlm", "lash_protocol_rlm", True),
    "lash_remote_protocol": ("lash-internal-remote-protocol", "lash_remote_protocol", True),
    "lash_rlm_types": ("lash-internal-rlm-types", "lash_rlm_types", True),
    "lash_sansio": ("lash-internal-sansio", "lash_sansio", True),
    "lash_tool_support": ("lash-internal-tool-support", "lash_tool_support", True),
    "lash_trace": ("lash-internal-trace", "lash_trace", True),
    "lashlang": ("lash-internal-lashlang", "lashlang", True),
    "schemars": ("schemars@0.8", "schemars", False),
    "schemars_derive": ("schemars_derive@0.8", "schemars_derive", False),
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

    This is the drift guard for `gated_core_modules`, so it has to recognize
    every shape rustdoc has used to say it.  Older formats wrote the attribute
    as a bare string or as a `{"doc_hidden": ...}` tag; the current one wraps
    unparsed attributes as `{"other": "#[doc(hidden)]"}`, which a key lookup
    misses -- and a guard that silently answers "no" to every item is not a
    guard.  Any string in the attribute, key or value, therefore decides.
    """
    for attribute in item.get("attrs") or []:
        if isinstance(attribute, dict):
            texts = [*attribute.keys(), *(
                value for value in attribute.values() if isinstance(value, str)
            )]
        else:
            texts = [attribute]
        for text in texts:
            if not isinstance(text, str):
                continue
            collapsed = text.replace(" ", "")
            if "doc(hidden)" in collapsed or "doc_hidden" in collapsed:
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
            if public(field["visibility"]):
                name = field.get("name") or field_id
                surface[(f"{path}::{name}", "field")] = f"{canonical}::{name}"

    if kind == "trait":
        for member_id in inner["items"]:
            member = index[str(member_id)]
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
            if variant is None:
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
        if field is None:
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
    document: dict[str, Any],
    all_features: bool,
    gated_modules: set[str] | None = None,
) -> dict[tuple[str, str], str]:
    """Root host exports plus every core-owned type they make reachable.

    Root exports are the named surface; the reachability closure is the rest of
    the contract. Nested public modules are not walked -- an item there enters
    the gate only when a publicly reachable signature exposes it, which is the
    same thing as being host-visible.

    `gated_modules` are the exception: the crate root's internal support modules,
    walked in full because they are the workspace's cross-crate API and the
    ledger answers for them like any other path. The set is recorded in the
    inventory rather than derived from `#[doc(hidden)]` alone, so neither adding
    a hidden support module nor un-hiding an existing one changes what the gate
    covers without an explicit edit: an unlisted hidden root module is an error,
    and a listed module that has vanished is an error too -- judged against the
    all-features document, so that a feature-gated support module is allowed to
    be absent from the default pass without reading as retired.
    """
    index = document["index"]
    paths = document["paths"]
    root = index[str(document["root"])]["inner"]["module"]
    surface: dict[tuple[str, str], str] = {}
    seeds: set[str] = set()
    gated = set(gated_modules or ())
    walked: set[str] = set()
    unlisted: list[str] = []
    for child_id in root["items"]:
        child = index[str(child_id)]
        if not public(child["visibility"]):
            continue
        if item_kind(child) == "module":
            name = exported_name(child)
            if name in gated:
                walked.add(name)
                add_export(
                    surface,
                    export_path("lash_core", child),
                    child,
                    index,
                    paths,
                    all_features,
                )
            elif doc_hidden(child):
                unlisted.append(name)
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

    if unlisted:
        raise AssertionError(
            "doc-hidden lash-core root modules outside the gate: "
            f"{', '.join(sorted(unlisted))}. Hidden is a documentation choice, "
            "not a ledger exemption -- add each to gated_core_modules in "
            f"{INVENTORY.name} and disposition its paths."
        )
    # Only the all-features pass answers for existence: a support module may be
    # feature-gated (`test_support` is `#[cfg(any(test, feature = "testing"))]`),
    # and absent-without-the-feature is what that module is supposed to be, not
    # a retirement. Retiring one still errors, because it is then missing from
    # this pass too.
    missing = sorted(gated - walked) if all_features else []
    if missing:
        raise AssertionError(
            f"gated_core_modules names modules lash-core no longer exports: "
            f"{', '.join(missing)}. Retiring a support module retires every path "
            "it published; record that in the inventory rather than here."
        )

    for path, item_id in core_reachable_types(seeds, index, paths).items():
        # A closure type is keyed by its canonical path, so path *is* identity.
        surface[(path, item_kind(index[item_id]))] = path
        add_members(surface, path, path, item_id, index)
    return surface


def rustdoc(
    package: str, crate_name: str, all_features: bool, hidden_items: bool = True
) -> dict[str, Any]:
    command = ["cargo", "rustdoc", "-p", package]
    if all_features:
        command.append("--all-features")
    command += [
        "--lib",
        "--",
        "-Z",
        "unstable-options",
        "--output-format",
        "json",
    ]
    if hidden_items:
        # Hidden items are still contract; the gate decides what to do with
        # them, and it cannot decide about what the compiler never names.
        command.append("--document-hidden-items")
    env = os.environ.copy()
    env["RUSTC_BOOTSTRAP"] = "1"
    # The isolation travels in the environment rather than as `--target-dir` on
    # the command line, and deliberately: the cache keys the command verbatim,
    # so an absolute target path inside it would give every checkout a key of
    # its own and cost the cache the cross-worktree reuse it exists for. What
    # the command names -- package, features, rustdoc flags -- is exactly what
    # decides the document, and that is what stays keyed.
    env["CARGO_TARGET_DIR"] = str(GATE_TARGET)
    # Generation is the whole cost of this gate on an unchanged tree; the walk
    # below still runs, against whichever copy of the compiler's answer the
    # cache hands back.
    document = rustdoc_json_cache.ensure(
        repo=REPO,
        package=package,
        crate_name=crate_name,
        command=command,
        destination=GATE_TARGET / "doc" / f"{crate_name}.json",
        generate=lambda: rustdoc_json_cache.run_command(command, cwd=REPO, env=env),
    )
    with document.open("rb") as handle:
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
            package_name,
            rustdoc_name,
            configured_all_features,
            hidden_items=supports_all_features,
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


def inventory_document() -> dict[str, Any]:
    with INVENTORY.open("rb") as handle:
        return tomllib.load(handle)


def gated_core_modules(document: dict[str, Any] | None = None) -> set[str]:
    """The `lash-core` root support modules the ledger answers for.

    Recorded rather than derived so that both directions of drift are explicit:
    a new hidden support module has to be added here before it can carry rows,
    and un-hiding one does not quietly drop the paths it already published.
    """
    inventory = document if document is not None else inventory_document()
    return set(inventory.get("gated_core_modules", []))


def configured_surface(all_features: bool) -> dict[tuple[str, str], str]:
    surface = lash_surface(
        rustdoc("lash-runtime", "lash", all_features), all_features
    )
    core = lash_core_surface(
        rustdoc("lash-internal-core", "lash_core", all_features),
        all_features,
        gated_core_modules(),
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


def canonical_paths(items: list[ApiItem] | None = None) -> list[str]:
    """Return every public facade spelling in deterministic order."""
    discovered = current_surface() if items is None else items
    return sorted(
        {
            path
            for item in discovered
            for path in item.paths
            if path.startswith("lash::")
        }
    )


def snapshot_text(items: list[ApiItem] | None = None) -> str:
    """Serialize the facade path set as one newline-terminated path per row."""
    paths = canonical_paths(items)
    return "".join(f"{path}\n" for path in paths)


def generate(snapshot: Path | None = None) -> int:
    """Regenerate the checked-in facade surface snapshot."""
    destination = SNAPSHOT if snapshot is None else snapshot
    text = snapshot_text()
    destination.write_text(text, encoding="utf-8")
    print(f"wrote {len(text.splitlines())} facade paths to {destination}")
    return 0


def check_snapshot(snapshot: Path | None = None) -> int:
    """Fail when the compiler-derived facade paths differ from the snapshot."""
    destination = SNAPSHOT if snapshot is None else snapshot
    recorded = destination.read_text(encoding="utf-8") if destination.exists() else ""
    current = snapshot_text()
    if recorded == current:
        print(f"API surface snapshot passed ({len(current.splitlines())} paths)")
        return 0

    print("API surface snapshot differs:", file=sys.stderr)
    for line in difflib.unified_diff(
        recorded.splitlines(),
        current.splitlines(),
        fromfile=str(destination.relative_to(REPO))
        if destination.is_relative_to(REPO)
        else str(destination),
        tofile="compiler-derived facade surface",
        lineterm="",
    ):
        print(line, file=sys.stderr)
    print(
        "Regenerate with: python3 scripts/api_surface.py generate",
        file=sys.stderr,
    )
    return 1


def dump_surface() -> int:
    """Print the full item projection consumed by the disposition checker."""
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
    commands = parser.add_subparsers(dest="command", required=True)
    commands.add_parser("generate", help="write docs/api-surface.snapshot")
    commands.add_parser("check", help="compare the snapshot with rustdoc")
    commands.add_parser("dump-surface", help="print the full surface as JSON")
    args = parser.parse_args()
    if args.command == "generate":
        return generate()
    if args.command == "check":
        return check_snapshot()
    return dump_surface()


if __name__ == "__main__":
    raise SystemExit(main())
