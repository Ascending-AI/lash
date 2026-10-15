#!/usr/bin/env python3
"""The transcript launcher must work on CI hosts with Buck2 config but no Kiln."""
import importlib.util
from pathlib import Path
import sys
import tempfile
import types
import unittest
from unittest import mock


class BuildStarted(Exception):
    pass


class TranscriptBuildDispatchTests(unittest.TestCase):
    def test_buckconfig_without_kiln_uses_the_repository_build_driver(self):
        browser_api = types.ModuleType("playwright.sync_api")
        browser_api.sync_playwright = mock.Mock()
        spec = importlib.util.spec_from_file_location(
            "transcript_projection_e2e",
            Path(__file__).with_name("workbench-transcript-projection-e2e.py"),
        )
        launcher = importlib.util.module_from_spec(spec)
        with mock.patch.dict(sys.modules, {"playwright.sync_api": browser_api}):
            spec.loader.exec_module(launcher)

        def start_build(command, **kwargs):
            if command[0] == "kiln":
                raise FileNotFoundError("kiln is not installed on this CI host")
            self.assertEqual(command[:3], ["bash", "scripts/hermetic-build.sh", "build"])
            raise BuildStarted

        with tempfile.TemporaryDirectory() as directory:
            launcher.ROOT = Path(directory)
            (launcher.ROOT / ".buckconfig.local").touch()
            with mock.patch("shutil.which", return_value=None), \
                    mock.patch.object(launcher.subprocess, "run", side_effect=start_build), \
                    self.assertRaises(BuildStarted):
                launcher.main()
        browser_api.sync_playwright.assert_not_called()


if __name__ == "__main__":
    unittest.main()
