#!/usr/bin/env python3
"""The release packager's actual plan stays within the SDK/operator boundary."""
from __future__ import annotations

import importlib.util
import pathlib
import tomllib
import unittest

ROOT = pathlib.Path(__file__).resolve().parent.parent


def release_artifact_manifest_contains_only_sdk_and_operator_binary():
    spec = importlib.util.spec_from_file_location(
        "package_workspace", ROOT / "scripts/package_workspace.py"
    )
    packager = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(packager)
    publisher = packager.load_publish_workspace()
    manifest = publisher.load_publishable_workspace_packages()
    metadata = publisher.run_json(
        ["cargo", "metadata", "--format-version", "1", "--locked", "--no-deps"]
    )
    packages = {package["id"]: package for package in metadata["packages"]}
    case = unittest.TestCase()
    case.assertTrue(manifest)
    case.assertIn("lash-runtime", {item["name"] for item in manifest.values()})
    for package_id in manifest:
        package = packages[package_id]
        directory = pathlib.Path(package["manifest_path"]).parent.relative_to(ROOT)
        case.assertEqual(directory.parts[0], "crates", package["name"])
        case.assertTrue(
            any("lib" in target["kind"] for target in package["targets"]),
            f"release package is not an SDK library: {package['name']}",
        )
    operator = tomllib.loads((ROOT / "crates/lashctl/Cargo.toml").read_text())
    case.assertFalse(operator["package"]["autobins"])
    case.assertEqual([target["name"] for target in operator["bin"]], ["lashctl"])
    workflow = (ROOT / ".github/workflows/release.yml").read_text()
    case.assertIn("python3 scripts/package_workspace.py", workflow)
    case.assertIn("python3 .release-tools/scripts/publish_workspace.py", workflow)
    # GitHub release assets are a distinct publication route. SDK crates may
    # contain schema-generator sources; no compiled generator is a release asset.
    release_action = workflow.split("uses: softprops/action-gh-release@", 1)[1]
    case.assertNotIn("files:", release_action)
    case.assertNotIn("target/release/", release_action)


def load_tests(loader, tests, pattern):
    tests.addTest(unittest.FunctionTestCase(
        release_artifact_manifest_contains_only_sdk_and_operator_binary
    ))
    return tests


if __name__ == "__main__":
    unittest.main()
