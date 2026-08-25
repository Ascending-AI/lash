#!/usr/bin/env python3

import contextlib
import io
from pathlib import Path
import tempfile
import unittest
from unittest import mock

import api_surface
from api_surface import ApiItem


class SnapshotTests(unittest.TestCase):
    ITEM = ApiItem(
        primary="lash::Session",
        kind="struct",
        availability="default+all-features",
        paths=["lash::Session", "lash::prelude::Session", "lash_core::Session"],
        identity="lash_core::session::Session",
    )

    def test_generate_then_check_round_trips_and_added_path_fails(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            snapshot = Path(directory) / "api-surface.snapshot"
            with mock.patch.object(
                api_surface,
                "canonical_paths",
                return_value=["lash::Session", "lash::prelude::Session"],
            ):
                self.assertEqual(api_surface.generate(snapshot), 0)
                self.assertEqual(api_surface.check_snapshot(snapshot), 0)

            with mock.patch.object(
                api_surface,
                "canonical_paths",
                return_value=[
                    "lash::Session",
                    "lash::SyntheticAddition",
                    "lash::prelude::Session",
                ]
            ):
                errors = io.StringIO()
                with contextlib.redirect_stderr(errors):
                    self.assertEqual(api_surface.check_snapshot(snapshot), 1)
                self.assertIn("+lash::SyntheticAddition", errors.getvalue())
                self.assertIn(
                    "python3 scripts/api_surface.py generate", errors.getvalue()
                )

    def test_snapshot_contains_only_sorted_facade_paths(self) -> None:
        self.assertEqual(
            api_surface.canonical_paths([self.ITEM]),
            ["lash::Session", "lash::prelude::Session"],
        )


if __name__ == "__main__":
    unittest.main()
