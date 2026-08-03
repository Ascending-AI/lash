#!/usr/bin/env python3

import unittest

from check_core_public_member_docs import undocumented_root_members


def item(name, visibility, inner, item_id):
    return {"id": item_id, "name": name, "visibility": visibility, "inner": inner}


def fixture(member_docs):
    return {
        "root": 0,
        "index": {
            "0": item("lash_core", "public", {"module": {"items": [1, 8]}}, 0),
            "1": item("Visible", "public", {"use": {"id": 2, "source": "model::Visible"}}, 1),
            "2": item("Visible", "public", {"struct": {"impls": [3, 6]}}, 2),
            "3": item(None, "public", {"impl": {"trait": None, "items": [4, 5]}}, 3),
            "4": {
                **item("documented", "public", {"function": {}}, 4),
                "docs": "Integrator contract.",
            },
            "5": {
                **item("candidate", "public", {"function": {}}, 5),
                "docs": member_docs,
            },
            "6": item(
                None,
                "public",
                {"impl": {"trait": {"path": "SomeTrait"}, "items": [7]}},
                6,
            ),
            "7": {**item("trait_method", "public", {"function": {}}, 7), "docs": None},
            # A foreign re-export is absent from this crate's index and is out of scope.
            "8": item("Foreign", "public", {"use": {"id": 99, "source": "dep::Foreign"}}, 8),
        },
    }


class CorePublicMemberDocsTests(unittest.TestCase):
    def test_reports_only_undocumented_core_owned_inherent_members(self):
        inspected, missing = undocumented_root_members(fixture(None))
        self.assertEqual(inspected, 2)
        self.assertEqual(missing, ["Visible::candidate"])

    def test_accepts_nonempty_member_docs(self):
        inspected, missing = undocumented_root_members(fixture("Caller and role."))
        self.assertEqual(inspected, 2)
        self.assertEqual(missing, [])


if __name__ == "__main__":
    unittest.main()
