#!/usr/bin/env python3

import unittest

from check_api_example_coverage import doc_hidden, lash_core_surface


def item(name, visibility, inner, item_id, **extra):
    return {
        "id": item_id,
        "name": name,
        "visibility": visibility,
        "inner": inner,
        **extra,
    }


def resolved(item_id):
    return {"resolved_path": {"path": "Ignored", "id": item_id, "args": None}}


def sig(output=None, inputs=()):
    return {"sig": {"inputs": list(inputs), "output": output, "is_c_variadic": False}}


def fixture():
    """A root export that hands out a `pub(crate)`-rooted type.

    `Handle` is the only named export. `Handed` is reachable because
    `Handle::handed` returns it, yet its module is crate-private, so no module
    walk can ever name it. `Hidden` is doc-hidden, and `Unreached` lives in a
    public module nothing reachable mentions.
    """
    return {
        "root": 0,
        "index": {
            "0": item("lash_core", "public", {"module": {"items": [1, 20]}}, 0),
            # ── root export ──
            "1": item(
                "Handle",
                "public",
                {"use": {"id": 2, "source": "h::Handle", "is_glob": False}},
                1,
            ),
            "2": item("Handle", "public", {"struct": {"kind": "unit", "impls": [3]}}, 2),
            "3": item(None, "public", {"impl": {"trait": None, "items": [4, 5]}}, 3),
            "4": item("handed", "public", {"function": sig(resolved(6))}, 4),
            "5": item(
                "hidden_handed",
                "public",
                {"function": sig(resolved(10))},
                5,
                attrs=["#[doc(hidden)]"],
            ),
            # ── reachable-only type, plus its own members ──
            "6": item(
                "Handed",
                "public",
                {"struct": {"kind": {"plain": {"fields": [8]}}, "impls": [7]}},
                6,
            ),
            "7": item(None, "public", {"impl": {"trait": None, "items": [9]}}, 7),
            "8": item("slot", "public", {"struct_field": {"primitive": "u64"}}, 8),
            "9": item("callable", "public", {"function": sig()}, 9),
            # ── doc-hidden type, returned only by a doc-hidden member ──
            "10": item("Hidden", "public", {"struct": {"kind": "unit", "impls": []}}, 10),
            # ── public module nothing reachable mentions ──
            "20": item("internals", "public", {"module": {"items": [21]}}, 20),
            "21": item("Unreached", "public", {"struct": {"kind": "unit", "impls": []}}, 21),
        },
        "paths": {
            "6": {"path": ["lash_core", "private_mod", "Handed"], "kind": "struct"},
            "10": {"path": ["lash_core", "private_mod", "Hidden"], "kind": "struct"},
        },
    }


class DocHiddenTests(unittest.TestCase):
    def test_recognizes_both_recorded_attribute_shapes(self):
        self.assertTrue(doc_hidden({"attrs": ["#[doc(hidden)]"]}))
        self.assertTrue(doc_hidden({"attrs": ["#[doc( hidden )]"]}))
        self.assertTrue(doc_hidden({"attrs": [{"doc_hidden": None}]}))
        self.assertFalse(doc_hidden({"attrs": ["#[doc = \"hidden costs\"]"]}))
        self.assertFalse(doc_hidden({}))


class CoreSurfaceTests(unittest.TestCase):
    def setUp(self):
        self.surface = lash_core_surface(fixture(), False)

    def test_names_the_root_export_and_its_members(self):
        self.assertIn(("lash_core::Handle", "struct"), self.surface)
        self.assertIn(("lash_core::Handle::handed", "function"), self.surface)

    def test_enumerates_a_pub_crate_rooted_reachable_type_by_canonical_path(self):
        # FIG-937: the Session-class hole. `Handed` has no nameable path, but a
        # host holds the value and can call everything on it.
        self.assertIn(("lash_core::private_mod::Handed", "struct"), self.surface)
        self.assertIn(("lash_core::private_mod::Handed::slot", "field"), self.surface)
        self.assertIn(("lash_core::private_mod::Handed::callable", "function"), self.surface)

    def test_excludes_doc_hidden_members_and_what_only_they_expose(self):
        self.assertNotIn(("lash_core::Handle::hidden_handed", "function"), self.surface)
        self.assertNotIn(("lash_core::private_mod::Hidden", "struct"), self.surface)

    def test_excludes_unreached_public_module_internals(self):
        self.assertEqual(
            [symbol for symbol, _ in self.surface if "Unreached" in symbol], []
        )


if __name__ == "__main__":
    unittest.main()
