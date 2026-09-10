#!/usr/bin/env python3

from __future__ import annotations

import json
import os
from pathlib import Path
import subprocess
import tempfile
import unittest

import ci_workspace_cache


class WorkspaceCacheMetadataTests(unittest.TestCase):
    def setUp(self) -> None:
        self.temporary = tempfile.TemporaryDirectory()
        self.root = Path(self.temporary.name)
        subprocess.run(["git", "init", "-q"], cwd=self.root, check=True)
        (self.root / "unchanged.rs").write_text("fn unchanged() {}\n", encoding="utf-8")
        (self.root / "changed.rs").write_text("fn before() {}\n", encoding="utf-8")
        subprocess.run(
            ["git", "add", "unchanged.rs", "changed.rs"],
            cwd=self.root,
            check=True,
        )
        os.utime(self.root / "unchanged.rs", ns=(100, 100))
        os.utime(self.root / "changed.rs", ns=(200, 200))
        self.manifest = self.root / "target" / "source-mtimes.json"
        ci_workspace_cache.snapshot(self.root, self.manifest)

    def tearDown(self) -> None:
        self.temporary.cleanup()

    def test_restore_only_rewinds_content_identical_files(self) -> None:
        unchanged = self.root / "unchanged.rs"
        changed = self.root / "changed.rs"
        changed.write_text("fn after() {}\n", encoding="utf-8")
        os.utime(unchanged, ns=(1000, 1000))
        os.utime(changed, ns=(1000, 1000))

        restored = ci_workspace_cache.restore(self.root, self.manifest)

        self.assertEqual(restored, 1)
        self.assertEqual(unchanged.stat().st_mtime_ns, 100)
        self.assertEqual(changed.stat().st_mtime_ns, 1000)

    def test_digest_uses_contents_not_mtimes(self) -> None:
        before = ci_workspace_cache.workspace_digest(self.root)
        os.utime(self.root / "unchanged.rs", ns=(5000, 5000))
        self.assertEqual(ci_workspace_cache.workspace_digest(self.root), before)
        (self.root / "unchanged.rs").write_text("fn different() {}\n", encoding="utf-8")
        self.assertNotEqual(ci_workspace_cache.workspace_digest(self.root), before)

    def test_manifest_cannot_escape_the_workspace(self) -> None:
        outside = self.root.parent / "outside.rs"
        outside.write_text("outside\n", encoding="utf-8")
        self.addCleanup(outside.unlink)
        self.manifest.write_text(
            json.dumps(
                {
                    "version": 1,
                    "files": {
                        "../outside.rs": {
                            "sha256": ci_workspace_cache.content_digest(outside),
                            "mtime_ns": 1,
                        }
                    },
                }
            ),
            encoding="utf-8",
        )
        with self.assertRaises(ValueError):
            ci_workspace_cache.restore(self.root, self.manifest)

    def test_missing_manifest_is_a_cache_miss(self) -> None:
        self.manifest.unlink()
        self.assertEqual(ci_workspace_cache.restore(self.root, self.manifest), 0)


if __name__ == "__main__":
    unittest.main()
