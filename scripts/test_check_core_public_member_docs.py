#!/usr/bin/env python3

import contextlib
import io
import json
from pathlib import Path
import tempfile
import unittest
import unittest.mock

from check_core_public_member_docs import (
    MINIMUM_INSPECTED_MEMBERS,
    main,
    undocumented_root_members,
)


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


def synthetic_document(member_count):
    """A lash-core-shaped document carrying `member_count` documented members."""
    members = list(range(100, 100 + member_count))
    index = {
        "0": item("lash_core", "public", {"module": {"items": [1]}}, 0),
        "1": item("Visible", "public", {"use": {"id": 2, "source": "model::Visible"}}, 1),
        "2": item("Visible", "public", {"struct": {"impls": [3]}}, 2),
        "3": item(None, "public", {"impl": {"trait": None, "items": members}}, 3),
    }
    for identifier in members:
        index[str(identifier)] = {
            **item(f"member_{identifier}", "public", {"function": {}}, identifier),
            "docs": "Integrator contract.",
        }
    return {"root": 0, "index": index}


class CorePublicMemberDocsTests(unittest.TestCase):
    def test_reports_only_undocumented_core_owned_inherent_members(self):
        inspected, missing = undocumented_root_members(fixture(None))
        self.assertEqual(inspected, 2)
        self.assertEqual(missing, ["Visible::candidate"])

    def test_accepts_nonempty_member_docs(self):
        inspected, missing = undocumented_root_members(fixture("Caller and role."))
        self.assertEqual(inspected, 2)
        self.assertEqual(missing, [])


class NonVacuityFloorTests(unittest.TestCase):
    """A document that inspects nothing must fail rather than pass silently."""

    def check(self, document):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "lash_core.json"
            path.write_text(json.dumps(document), encoding="utf-8")
            with unittest.mock.patch("sys.argv", ["check", str(path)]):
                with contextlib.redirect_stdout(io.StringIO()) as reported:
                    status = main()
        self.reported = reported.getvalue()
        return status

    def test_empty_root_module_fails_instead_of_reporting_zero_of_zero(self):
        empty = {
            "root": 0,
            "index": {"0": item("lash_core", "public", {"module": {"items": []}}, 0)},
        }
        self.assertEqual(self.check(empty), 1)
        self.assertIn("0 member(s) inspected", self.reported)

    def test_a_document_just_under_the_floor_fails(self):
        self.assertEqual(self.check(synthetic_document(MINIMUM_INSPECTED_MEMBERS - 1)), 1)

    def test_a_document_at_the_floor_passes(self):
        self.assertEqual(self.check(synthetic_document(MINIMUM_INSPECTED_MEMBERS)), 0)


if __name__ == "__main__":
    unittest.main()
