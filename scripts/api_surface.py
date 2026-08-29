#!/usr/bin/env python3
"""Discover and snapshot Lash's compiler-derived facade API surface."""

from __future__ import annotations

import argparse
import difflib
import json
import os
from pathlib import Path
import subprocess
import sys
from typing import Any, NamedTuple


SCRIPTS = Path(__file__).resolve().parent
if str(SCRIPTS) not in sys.path:
    sys.path.insert(0, str(SCRIPTS))

import rustdoc_json_cache  # noqa: E402  (sibling script)

REPO = Path(__file__).resolve().parents[1]
SNAPSHOT = REPO / "docs" / "api-surface.snapshot"


def target_directory(environment: dict[str, str] | None = None) -> Path:
    """Return Cargo's target directory for this checkout as an absolute path."""
    if environment is None:
        environment = dict(os.environ)
    return REPO / Path(environment.get("CARGO_TARGET_DIR") or "target")


TARGET = target_directory()
FACADE_GATE_TARGET = TARGET / "facade-gate"

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

# Crates whose rustdoc JSON may be needed to resolve facade re-exports. The
# boolean says whether the package supports the all-features hidden-item pass.
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
_METADATA: dict[str, Any] = {}


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


def canonical_identity(
    item_id: str | None, paths: dict[str, dict[str, Any]], fallback: str
) -> str:
    entry = paths.get(str(item_id)) if item_id is not None else None
    if entry is None or not entry.get("path"):
        return fallback
    return "::".join(entry["path"])


def add_members(
    surface: dict[tuple[str, str], str],
    path: str,
    canonical: str,
    item_id: str,
    index: dict[str, dict[str, Any]],
) -> None:
    """Record public fields, variants, and inherent members of an exported type."""
    item = index.get(item_id)
    if item is None:
        return
    kind = item_kind(item)
    inner = item["inner"][kind]

    if kind == "enum":
        for variant_id in inner["variants"]:
            variant = index.get(str(variant_id))
            if variant is None:
                continue
            variant_path = f"{path}::{variant['name']}"
            variant_canonical = f"{canonical}::{variant['name']}"
            surface[(variant_path, "variant")] = variant_canonical
            shape = variant["inner"]["variant"]["kind"]
            field_ids: list[Any] = []
            if isinstance(shape, dict) and "struct" in shape:
                field_ids = shape["struct"]["fields"]
            elif isinstance(shape, dict) and "tuple" in shape:
                field_ids = [field for field in shape["tuple"] if field is not None]
            for field_id in field_ids:
                field = index.get(str(field_id))
                if field is None:
                    continue
                name = field.get("name") or field_id
                surface[(f"{variant_path}::{name}", "field")] = (
                    f"{variant_canonical}::{name}"
                )
    elif kind in {"struct", "union"}:
        shape = inner["kind"] if kind == "struct" else inner
        field_ids: list[Any] = []
        if isinstance(shape, dict) and "plain" in shape:
            field_ids = shape["plain"]["fields"]
        elif isinstance(shape, dict) and "tuple" in shape:
            field_ids = [field for field in shape["tuple"] if field is not None]
        elif kind == "union":
            field_ids = inner["fields"]
        for field_id in field_ids:
            field = index.get(str(field_id))
            if field is not None and public(field["visibility"]):
                name = field.get("name") or field_id
                surface[(f"{path}::{name}", "field")] = f"{canonical}::{name}"

    members: list[dict[str, Any]] = []
    if kind == "trait":
        members.extend(
            member
            for member_id in inner["items"]
            if (member := index.get(str(member_id))) is not None
        )
    for impl_id in inner.get("impls", []) if isinstance(inner, dict) else []:
        implementation = index.get(str(impl_id))
        if implementation is None:
            continue
        impl = implementation["inner"].get("impl")
        if impl is None or impl["trait"] is not None:
            continue
        members.extend(
            member
            for member_id in impl["items"]
            if (member := index.get(str(member_id))) is not None
            and public(member["visibility"])
        )
    for member in members:
        name = member.get("name")
        if name is not None:
            surface[(f"{path}::{name}", item_kind(member))] = f"{canonical}::{name}"


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
            if public(child["visibility"]):
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


def public_surface(
    document: dict[str, Any],
    root_path: str,
    all_features: bool,
    *,
    excluded_root_exports: frozenset[str] = frozenset(),
) -> dict[tuple[str, str], str]:
    """Walk public exports from a crate root using the facade identity rules."""
    index = document["index"]
    paths = document["paths"]
    surface: dict[tuple[str, str], str] = {}

    def walk(module_id: str, prefix: str, *, at_root: bool = False) -> None:
        for child_id in index[module_id]["inner"]["module"]["items"]:
            child = index[str(child_id)]
            if not public(child["visibility"]):
                continue
            if at_root and exported_name(child) in excluded_root_exports:
                continue
            path = export_path(prefix, child)
            if item_kind(child) == "module":
                walk(str(child_id), path)
            else:
                add_export(surface, path, child, index, paths, all_features)

    walk(str(document["root"]), root_path, at_root=True)
    return surface


def lash_surface(
    document: dict[str, Any], all_features: bool
) -> dict[tuple[str, str], str]:
    """Walk every public export in the app-facing `lash` crate."""
    return public_surface(document, "lash", all_features)


def referenced_ids(node: Any, found: set[str]) -> None:
    """Collect resolved item ids from a rustdoc type or generics node."""
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
    """Return item ids exposed through a facade export's public signature."""
    item = index.get(item_id)
    if item is None:
        return set()
    kind = item_kind(item)
    if kind not in {"enum", "struct", "trait", "type_alias", "union"}:
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
                field_ids.extend(shape["struct"]["fields"])
            elif isinstance(shape, dict) and "tuple" in shape:
                field_ids.extend(field for field in shape["tuple"] if field is not None)
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
        if field is not None:
            referenced_ids(field["inner"]["struct_field"], found)
    members: list[dict[str, Any]] = []
    if kind == "trait":
        members.extend(
            member
            for member_id in inner["items"]
            if (member := index.get(str(member_id))) is not None
        )
    for impl_id in inner.get("impls", []) if isinstance(inner, dict) else []:
        implementation = index.get(str(impl_id))
        if implementation is None:
            continue
        impl = implementation["inner"].get("impl")
        if impl is None or impl["trait"] is not None:
            continue
        members.extend(
            member
            for member_id in impl["items"]
            if (member := index.get(str(member_id))) is not None
            and public(member["visibility"])
        )
    for member in members:
        found |= signature_ids(member)
    return found


def rustdoc(
    package: str, crate_name: str, all_features: bool, hidden_items: bool = True
) -> dict[str, Any]:
    command = ["cargo", "rustdoc", "-p", package]
    if all_features:
        command.append("--all-features")
    command += ["--lib", "--", "-Z", "unstable-options", "--output-format", "json"]
    if hidden_items:
        command.append("--document-hidden-items")
    environment = os.environ.copy()
    environment["RUSTC_BOOTSTRAP"] = "1"
    environment["CARGO_TARGET_DIR"] = str(FACADE_GATE_TARGET)
    document = rustdoc_json_cache.ensure(
        repo=REPO,
        package=package,
        crate_name=crate_name,
        command=command,
        destination=FACADE_GATE_TARGET / "doc" / f"{crate_name}.json",
        generate=lambda: rustdoc_json_cache.run_command(
            command, cwd=REPO, env=environment
        ),
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


def cargo_metadata() -> dict[str, Any]:
    if not _METADATA:
        completed = subprocess.run(
            ["cargo", "metadata", "--format-version", "1", "--all-features", "--locked"],
            cwd=REPO,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
        )
        _METADATA.update(json.loads(completed.stdout))
    return _METADATA


def crate_directories() -> dict[str, str]:
    """Return workspace library and proc-macro crate names mapped to directories."""
    metadata = cargo_metadata()
    members = set(metadata["workspace_members"])
    directories: dict[str, str] = {}
    for package in metadata["packages"]:
        if package["id"] not in members:
            continue
        manifest = Path(package["manifest_path"]).parent
        if not manifest.is_relative_to(REPO):
            continue
        directory = manifest.relative_to(REPO).as_posix()
        for target in package["targets"]:
            if "lib" in target["kind"] or "proc-macro" in target["kind"]:
                directories[target["name"].replace("-", "_")] = directory
    return directories


def api_items(surface: dict[tuple[str, str], str]) -> dict[tuple[str, str], list[str]]:
    grouped: dict[tuple[str, str], list[str]] = {}
    for (path, kind), identity in surface.items():
        grouped.setdefault((identity, kind), []).append(path)
    return {key: sorted(paths) for key, paths in grouped.items()}


def primary_path(paths: list[str]) -> str:
    return min(paths, key=lambda path: (path.count("::"), path))


class ApiItem(NamedTuple):
    primary: str
    kind: str
    availability: str
    paths: list[str]
    identity: str

    def aliases(self) -> list[str]:
        return [path for path in self.paths if path != self.primary]


def current_surface() -> list[ApiItem]:
    default = lash_surface(rustdoc("lash-runtime", "lash", False), False)
    all_features = lash_surface(rustdoc("lash-runtime", "lash", True), True)
    surface = {**all_features, **default}
    conflicts = sorted(
        key
        for key in set(default) & set(all_features)
        if default[key] != all_features[key]
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
        items.append(ApiItem(primary_path(paths), kind, availability, paths, identity))
    return sorted(items)


def canonical_paths(items: list[ApiItem] | None = None) -> list[str]:
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
    return "".join(f"{path}\n" for path in canonical_paths(items))


def generate(snapshot: Path | None = None) -> int:
    destination = SNAPSHOT if snapshot is None else snapshot
    text = snapshot_text()
    destination.write_text(text, encoding="utf-8")
    print(f"wrote {len(text.splitlines())} facade paths to {destination}")
    return 0


def check_snapshot(snapshot: Path | None = None) -> int:
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
    print("Regenerate with: python3 scripts/api_surface.py generate", file=sys.stderr)
    return 1


def dump_surface() -> int:
    """Print the compiler-derived facade item projection as JSON."""
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
    commands.add_parser("dump-surface", help="print the facade item projection")
    args = parser.parse_args()
    if args.command == "generate":
        return generate()
    if args.command == "check":
        return check_snapshot()
    return dump_surface()


if __name__ == "__main__":
    raise SystemExit(main())
