#!/usr/bin/env python3
"""Every first-party type a facade signature names is nameable through the facade.

A host depends on the facade crate alone (FIG-4373). An item the facade exports
is usable only if every first-party type its signature names -- function and
method parameters and returns, public fields, variant payloads, trait
supertraits, associated types and consts, generic bounds, type aliases -- has a
path under the facade. This walks rustdoc JSON documents (one per crate) and
fails listing each unreachable type with the facade items whose signatures name
it.

Reachability starts at the facade root and follows public modules and `pub use`
re-exports, including globs and re-exports from other crates; a re-export's
target is found in its own crate's document by definition path, which rustdoc
records identically in every document that mentions the item. Only first-party
crates (those with a document) are checked: `std` and third-party types are
their own crates' business.

Usage: facade_completeness.py --facade <crate_name> <document dir>...

Each document directory holds exactly one rustdoc JSON document, produced with
`RUSTC_BOOTSTRAP=1 rustdoc -Zunstable-options --output-format=json
--document-hidden-items --cap-lints=allow` for the facade crate or one
first-party library in its dependency closure, under the facade's resolved
features. Exit status: 0 when every first-party type is nameable (one summary
line on stdout); 1 when a type is not nameable or a re-export does not resolve
(one line per gap on stderr, then a summary), or when a directory does not hold
exactly one document or a crate is documented twice; 2 on a usage error.
"""

from __future__ import annotations

import argparse
import json
import sys
from collections import defaultdict
from dataclasses import dataclass, field
from pathlib import Path
from typing import Any, Iterator

# Item kinds whose signatures the check reads. Functions, traits, types with
# fields and variants, aliases and constants all name types a host must spell.
SIGNATURE_KINDS = frozenset(
    {
        "function",
        "struct",
        "enum",
        "union",
        "trait",
        "type_alias",
        "constant",
        "static",
        "variant",
    }
)


@dataclass(frozen=True)
class ItemRef:
    """An item in one crate's document, by that document's local id."""

    crate: str
    id: str


@dataclass
class Document:
    crate: str
    raw: dict[str, Any]
    # Definition path -> local ids, for resolving other crates' references.
    by_path: dict[tuple[str, ...], list[str]] = field(default_factory=dict)

    @property
    def index(self) -> dict[str, Any]:
        return self.raw["index"]

    @property
    def paths(self) -> dict[str, Any]:
        return self.raw["paths"]

    def crate_of(self, path_entry: dict[str, Any]) -> str:
        crate_id = path_entry["crate_id"]
        if crate_id == 0:
            return self.crate
        return self.raw["external_crates"][str(crate_id)]["name"]


def load_documents(dirs: list[Path]) -> dict[str, Document]:
    documents: dict[str, Document] = {}
    for directory in dirs:
        files = sorted(directory.glob("*.json"))
        if len(files) != 1:
            raise SystemExit(f"{directory}: expected one rustdoc JSON document, found {len(files)}")
        raw = json.loads(files[0].read_text(encoding="utf-8"))
        root = raw["index"][str(raw["root"])]
        crate = root["name"]
        if crate in documents:
            raise SystemExit(f"crate {crate} documented twice")
        document = Document(crate=crate, raw=raw)
        for item_id, entry in raw["paths"].items():
            if entry["crate_id"] == 0 and item_id in raw["index"]:
                document.by_path.setdefault(tuple(entry["path"]), []).append(item_id)
        documents[crate] = document
    return documents


def is_hidden(item: dict[str, Any]) -> bool:
    """Whether the item is `#[doc(hidden)]`: nameable, but not supported surface."""

    return any("doc(hidden)" in json.dumps(attr) for attr in item.get("attrs", []))


def kind_of(item: dict[str, Any]) -> str:
    (kind,) = item["inner"].keys()
    return kind


class Facade:
    def __init__(self, documents: dict[str, Document], facade: str) -> None:
        if facade not in documents:
            raise SystemExit(f"no rustdoc JSON document for the facade crate {facade}")
        self.documents = documents
        self.facade = facade
        # Items a `lash::` path reaches, and the first such path, for reports.
        self.reachable: dict[ItemRef, str] = {}
        # The reachable items some path reaches without passing a
        # `#[doc(hidden)]` item: the supported surface whose signatures count.
        # A hidden item is still nameable, so it satisfies a reference.
        self.supported: set[ItemRef] = set()
        self.unresolved_reexports: list[str] = []

    def resolve(self, document: Document, item_id: int | str) -> ItemRef | None:
        """The item `item_id` names in `document`, in the document that defines it."""

        key = str(item_id)
        if key in document.index:
            return ItemRef(document.crate, key)
        entry = document.paths.get(key)
        if entry is None:
            return None
        crate = document.crate_of(entry)
        target = self.documents.get(crate)
        if target is None:
            return None
        candidates = target.by_path.get(tuple(entry["path"]), [])
        for candidate in candidates:
            if kind_of(target.index[candidate]) == entry["kind"]:
                return ItemRef(crate, candidate)
        if candidates:
            return ItemRef(crate, candidates[0])
        return None

    def item(self, ref: ItemRef) -> dict[str, Any]:
        return self.documents[ref.crate].index[ref.id]

    def walk(self) -> None:
        root = self.documents[self.facade]
        stack: list[tuple[ItemRef, str, bool]] = [
            (ItemRef(self.facade, str(root.raw["root"])), self.facade, False)
        ]
        visited: set[tuple[ItemRef, bool]] = set()
        while stack:
            ref, path, hidden = stack.pop()
            item = self.item(ref)
            hidden = hidden or is_hidden(item)
            if (ref, hidden) in visited or (ref, False) in visited:
                continue
            visited.add((ref, hidden))
            self.reachable.setdefault(ref, path)
            if not hidden:
                self.supported.add(ref)
            inner = item["inner"]
            if "module" in inner:
                self._push_children(ref, inner["module"]["items"], path, hidden, stack)
            elif "enum" in inner:
                self._push_children(ref, inner["enum"]["variants"], path, hidden, stack)

    def _push_children(
        self,
        parent_ref: ItemRef,
        children: list[Any],
        path: str,
        hidden: bool,
        stack: list[tuple[ItemRef, str, bool]],
    ) -> None:
        document = self.documents[parent_ref.crate]
        for child in children:
            child_ref = self.resolve(document, child)
            if child_ref is None:
                continue
            child_item = self.item(child_ref)
            if "use" in child_item["inner"]:
                self._follow_use(child_ref, path, hidden or is_hidden(child_item), stack)
            elif child_item.get("name") is not None:
                stack.append((child_ref, f"{path}::{child_item['name']}", hidden))

    def _follow_use(
        self,
        use_ref: ItemRef,
        parent: str,
        hidden: bool,
        stack: list[tuple[ItemRef, str, bool]],
    ) -> None:
        use = self.item(use_ref)["inner"]["use"]
        if use["id"] is None:
            # A primitive or an unresolvable source: nothing first-party.
            return
        target = self.resolve(self.documents[use_ref.crate], use["id"])
        if target is None:
            entry = self.documents[use_ref.crate].paths.get(str(use["id"]))
            if entry is not None and self.documents[use_ref.crate].crate_of(entry) in self.documents:
                self.unresolved_reexports.append(f"{parent}: `pub use {use['source']}`")
            return
        if use["is_glob"]:
            # A glob names the target's children, not the target itself.
            target_inner = self.item(target)["inner"]
            if "module" in target_inner:
                self._push_children(target, target_inner["module"]["items"], parent, hidden, stack)
            elif "enum" in target_inner:
                self._push_children(target, target_inner["enum"]["variants"], parent, hidden, stack)
            return
        stack.append((target, f"{parent}::{use['name']}", hidden))

    # -- signatures ------------------------------------------------------------

    def signature_ids(self, ref: ItemRef) -> Iterator[tuple[Document, int]]:
        """Every type or trait path id the item's public signature names."""

        document = self.documents[ref.crate]
        item = self.item(ref)
        kind = kind_of(item)
        inner = item["inner"][kind]
        if kind == "function":
            yield from path_ids(document, inner["sig"])
            yield from path_ids(document, inner["generics"])
        elif kind in ("struct", "union"):
            yield from path_ids(document, inner["generics"])
            yield from self._fields(document, fields_of(kind, inner))
            yield from self._impls(document, inner["impls"])
        elif kind == "enum":
            yield from path_ids(document, inner["generics"])
            for variant in inner["variants"]:
                if str(variant) in document.index:
                    yield from self._variant(document, document.index[str(variant)])
            yield from self._impls(document, inner["impls"])
        elif kind == "variant":
            yield from self._variant(document, item)
        elif kind == "trait":
            yield from path_ids(document, inner["generics"])
            yield from path_ids(document, inner["bounds"])
            for member in inner["items"]:
                yield from self._member(document, member)
        elif kind == "type_alias":
            yield from path_ids(document, inner["type"])
            yield from path_ids(document, inner["generics"])
        elif kind in ("constant", "static"):
            yield from path_ids(document, inner["type"])

    def _fields(self, document: Document, field_ids: list[Any]) -> Iterator[tuple[Document, int]]:
        for field_id in field_ids:
            if field_id is None or str(field_id) not in document.index:
                continue
            yield from path_ids(document, document.index[str(field_id)]["inner"]["struct_field"])

    def _variant(self, document: Document, item: dict[str, Any]) -> Iterator[tuple[Document, int]]:
        variant_kind = item["inner"]["variant"]["kind"]
        if isinstance(variant_kind, dict):
            if "tuple" in variant_kind:
                yield from self._fields(document, variant_kind["tuple"])
            elif "struct" in variant_kind:
                yield from self._fields(document, variant_kind["struct"]["fields"])

    def _member(self, document: Document, member_id: Any) -> Iterator[tuple[Document, int]]:
        member = document.index.get(str(member_id))
        if member is None or is_hidden(member):
            return
        kind = kind_of(member)
        inner = member["inner"][kind]
        if kind == "function":
            yield from path_ids(document, inner["sig"])
            yield from path_ids(document, inner["generics"])
        elif kind == "assoc_type":
            yield from path_ids(document, inner["generics"])
            yield from path_ids(document, inner["bounds"])
            yield from path_ids(document, inner["type"])
        elif kind == "assoc_const":
            yield from path_ids(document, inner["type"])

    def _impls(self, document: Document, impl_ids: list[Any]) -> Iterator[tuple[Document, int]]:
        for impl_id in impl_ids:
            impl = document.index.get(str(impl_id))
            if impl is None or is_hidden(impl):
                continue
            inner = impl["inner"]["impl"]
            if inner["is_synthetic"] or inner["blanket_impl"] is not None:
                continue
            if inner["trait"] is None:
                # An inherent impl's public members are the type's API.
                for member in inner["items"]:
                    yield from self._member(document, member)
            else:
                # A trait impl's members are the trait's signatures; the impl
                # itself names the trait, its arguments and the associated
                # types it binds.
                yield from path_ids(document, inner["trait"])
                for member in inner["items"]:
                    bound = document.index.get(str(member))
                    if bound is not None and kind_of(bound) == "assoc_type":
                        yield from path_ids(document, bound["inner"]["assoc_type"]["type"])

    def gaps(self) -> dict[tuple[str, str], set[str]]:
        """Unreachable first-party (crate, definition path) -> facade items naming it."""

        missing: dict[tuple[str, str], set[str]] = defaultdict(set)
        for ref, facade_path in sorted(self.reachable.items(), key=lambda pair: pair[1]):
            if ref not in self.supported or kind_of(self.item(ref)) not in SIGNATURE_KINDS:
                continue
            for document, path_id in self.signature_ids(ref):
                entry = document.paths.get(str(path_id))
                if entry is None:
                    continue
                crate = document.crate_of(entry)
                if crate not in self.documents:
                    continue
                target = self.resolve(document, path_id)
                if target is not None and target in self.reachable:
                    continue
                missing[(crate, "::".join(entry["path"]))].add(facade_path)
        return missing


def fields_of(kind: str, inner: dict[str, Any]) -> list[Any]:
    if kind == "union":
        return inner["fields"]
    struct_kind = inner["kind"]
    if isinstance(struct_kind, dict):
        if "tuple" in struct_kind:
            return struct_kind["tuple"]
        if "plain" in struct_kind:
            return struct_kind["plain"]["fields"]
    return []


def path_ids(document: Document, value: Any) -> Iterator[tuple[Document, int]]:
    """Every rustdoc `Path` id inside a type, bound, generics or signature value."""

    if isinstance(value, dict):
        if isinstance(value.get("path"), str) and isinstance(value.get("id"), int):
            yield document, value["id"]
        for child in value.values():
            yield from path_ids(document, child)
    elif isinstance(value, list):
        for child in value:
            yield from path_ids(document, child)


def main(argv: list[str]) -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument("--facade", required=True, help="the facade crate name")
    parser.add_argument("documents", nargs="+", type=Path)
    args = parser.parse_args(argv)

    facade = Facade(load_documents(args.documents), args.facade)
    facade.walk()
    gaps = facade.gaps()
    for reexport in facade.unresolved_reexports:
        print(f"unresolved re-export {reexport}", file=sys.stderr)
    for (crate, path), users in sorted(gaps.items()):
        shown = sorted(users)
        more = f" (+{len(shown) - 3} more)" if len(shown) > 3 else ""
        print(
            f"not nameable through `{args.facade}::`: {path} (crate {crate}), "
            f"named by {', '.join(shown[:3])}{more}",
            file=sys.stderr,
        )
    if gaps or facade.unresolved_reexports:
        print(
            f"facade completeness: {len(gaps)} first-party type(s) named by facade signatures "
            f"have no `{args.facade}::` path; export each through the facade or narrow the "
            "signature that names it",
            file=sys.stderr,
        )
        return 1
    print(
        f"facade completeness: {len(facade.reachable)} reachable items across "
        f"{len(facade.documents)} crates; every first-party signature type is nameable "
        f"through `{args.facade}::`"
    )
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
