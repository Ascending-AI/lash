#!/usr/bin/env python3
"""Self-test for scripts/facade_completeness.py over synthetic rustdoc JSON.

The documents here carry only the fields the walker reads. Crate `a` stands in
for an internal crate and `f` for the facade that re-exports from it.
"""

from __future__ import annotations

import json
import sys
import tempfile
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parent))

import facade_completeness as fc  # noqa: E402

HIDDEN = [{"other": "#[doc(hidden)]"}]
NO_GENERICS = {"params": [], "where_predicates": []}


def ty(name: str, item_id: int) -> dict:
    return {"resolved_path": {"path": name, "id": item_id, "args": None}}


def module(name: str, items: list[int], attrs: list | None = None) -> dict:
    return {"name": name, "attrs": attrs or [], "inner": {"module": {"items": items}}}


def use(name: str, source: str, target: int, glob: bool = False, attrs: list | None = None) -> dict:
    return {
        "name": None,
        "attrs": attrs or [],
        "inner": {"use": {"source": source, "name": name, "id": target, "is_glob": glob}},
    }


def struct(name: str, impls: list[int], attrs: list | None = None) -> dict:
    return {
        "name": name,
        "attrs": attrs or [],
        "inner": {
            "struct": {
                "kind": {"plain": {"fields": [], "has_stripped_fields": False}},
                "generics": NO_GENERICS,
                "impls": impls,
            }
        },
    }


def function(name: str, inputs: list, output: dict | None, attrs: list | None = None) -> dict:
    return {
        "name": name,
        "attrs": attrs or [],
        "inner": {
            "function": {
                "sig": {"inputs": inputs, "output": output, "is_c_variadic": False},
                "generics": NO_GENERICS,
            }
        },
    }


def inherent_impl(items: list[int]) -> dict:
    return {
        "name": None,
        "attrs": [],
        "inner": {
            "impl": {
                "is_synthetic": False,
                "blanket_impl": None,
                "trait": None,
                "for": None,
                "items": items,
                "generics": NO_GENERICS,
            }
        },
    }


def trait(name: str, items: list[int]) -> dict:
    return {
        "name": name,
        "attrs": [],
        "inner": {"trait": {"generics": NO_GENERICS, "bounds": [], "items": items}},
    }


def crate_a() -> dict:
    """`a`: a trait whose method takes a context, a store with an inherent
    method returning another store, a hidden type, and a sealed-away type."""

    index = {
        "0": module("a", [1, 3, 5, 8, 10, 12]),
        # pub mod plugin { pub trait Factory { fn go(&self, ctx: &Ctx); } pub struct Ctx; }
        "1": module("plugin", [2, 4]),
        "2": trait("Factory", [6]),
        "4": struct("Ctx", []),
        "6": function("go", [["ctx", {"borrowed_ref": {"type": ty("Ctx", 4)}}]], None),
        # pub struct Stores { .. } impl Stores { pub fn definitions(&self) -> Definitions }
        "3": struct("Stores", [7]),
        "7": inherent_impl([9]),
        "9": function("definitions", [], ty("Definitions", 5)),
        "5": struct("Definitions", []),
        # #[doc(hidden)] pub struct Internal; impl Internal { pub fn private() -> Private }
        "8": struct("Internal", [11], attrs=HIDDEN),
        "11": inherent_impl([13]),
        "13": function("private", [], ty("Private", 10)),
        "10": struct("Private", []),
        # pub mod prelude { pub struct Globbed; }
        "12": module("prelude", [14]),
        "14": struct("Globbed", []),
    }
    paths = {
        "0": {"crate_id": 0, "path": ["a"], "kind": "module"},
        "1": {"crate_id": 0, "path": ["a", "plugin"], "kind": "module"},
        "2": {"crate_id": 0, "path": ["a", "plugin", "Factory"], "kind": "trait"},
        "4": {"crate_id": 0, "path": ["a", "plugin", "Ctx"], "kind": "struct"},
        "3": {"crate_id": 0, "path": ["a", "Stores"], "kind": "struct"},
        "5": {"crate_id": 0, "path": ["a", "store", "Definitions"], "kind": "struct"},
        "8": {"crate_id": 0, "path": ["a", "Internal"], "kind": "struct"},
        "10": {"crate_id": 0, "path": ["a", "Private"], "kind": "struct"},
        "12": {"crate_id": 0, "path": ["a", "prelude"], "kind": "module"},
        "14": {"crate_id": 0, "path": ["a", "prelude", "Globbed"], "kind": "struct"},
    }
    return {"root": 0, "index": index, "paths": paths, "external_crates": {}}


def crate_f(extra_exports: list[str]) -> dict:
    """The facade: `f::plugins::Factory`, `f::Stores`, `f::Internal`, a glob
    of `a::prelude`, a function naming `std` and `a::prelude::Globbed`, and
    whatever `extra_exports` adds at the root."""

    external = {"1": {"name": "a"}, "2": {"name": "std"}}
    targets = {
        "Factory": (100, ["a", "plugin", "Factory"], "trait"),
        "Ctx": (101, ["a", "plugin", "Ctx"], "struct"),
        "Stores": (102, ["a", "Stores"], "struct"),
        "Definitions": (103, ["a", "store", "Definitions"], "struct"),
        "Internal": (104, ["a", "Internal"], "struct"),
        "prelude": (105, ["a", "prelude"], "module"),
        "Globbed": (106, ["a", "prelude", "Globbed"], "struct"),
        "String": (107, ["std", "string", "String"], "struct"),
    }
    paths = {
        str(item_id): {"crate_id": 2 if name == "String" else 1, "path": path, "kind": kind}
        for name, (item_id, path, kind) in targets.items()
    }
    index = {
        "0": module("f", [1, 2, 3, 4, 5] + [50 + i for i in range(len(extra_exports))]),
        "1": module("plugins", [10]),
        "10": use("Factory", "a::plugin::Factory", targets["Factory"][0]),
        "2": use("Stores", "a::Stores", targets["Stores"][0]),
        "3": use("Internal", "a::Internal", targets["Internal"][0]),
        "4": use("prelude", "a::prelude", targets["prelude"][0], glob=True),
        "5": function(
            "describe",
            [["name", ty("String", 107)]],
            ty("Globbed", targets["Globbed"][0]),
        ),
    }
    paths["5"] = {"crate_id": 0, "path": ["f", "describe"], "kind": "function"}
    for offset, name in enumerate(extra_exports):
        item_id, path, _kind = targets[name]
        index[str(50 + offset)] = use(name, "::".join(path), item_id)
    return {"root": 0, "index": index, "paths": paths, "external_crates": external}


class FacadeCompletenessTest(unittest.TestCase):
    def gaps(self, extra_exports: list[str]) -> dict:
        with tempfile.TemporaryDirectory() as root:
            dirs = []
            for crate, document in (("a", crate_a()), ("f", crate_f(extra_exports))):
                directory = Path(root) / crate
                directory.mkdir()
                (directory / f"{crate}.json").write_text(json.dumps(document), encoding="utf-8")
                dirs.append(directory)
            facade = fc.Facade(fc.load_documents(dirs), "f")
            facade.walk()
            self.assertEqual([], facade.unresolved_reexports)
            return {path: sorted(users) for (_crate, path), users in facade.gaps().items()}

    def test_trait_parameters_and_inherent_returns_must_be_nameable(self) -> None:
        self.assertEqual(
            {
                "a::plugin::Ctx": ["f::plugins::Factory"],
                "a::store::Definitions": ["f::Stores"],
            },
            self.gaps([]),
        )

    def test_exporting_the_named_types_closes_the_gaps(self) -> None:
        self.assertEqual({}, self.gaps(["Ctx", "Definitions"]))

    def test_hidden_items_are_nameable_but_their_signatures_are_not_checked(self) -> None:
        # `a::Private` is named only by the hidden `f::Internal`'s method.
        self.assertNotIn("a::Private", self.gaps([]))

    def test_glob_reexports_reach_the_module_children(self) -> None:
        # `f::describe` returns `a::prelude::Globbed`, reachable by the glob.
        self.assertNotIn("a::prelude::Globbed", self.gaps([]))

    def test_types_outside_the_documented_crates_are_ignored(self) -> None:
        self.assertNotIn("std::string::String", self.gaps([]))


if __name__ == "__main__":
    unittest.main()
