#!/usr/bin/env python3
from __future__ import annotations

import contextlib
import hashlib
import importlib.util
import io
import pathlib
import subprocess
import tempfile
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parent.parent


def load_package_workspace_module():
    module_path = ROOT / "scripts" / "package_workspace.py"
    spec = importlib.util.spec_from_file_location("package_workspace", module_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def packages(*names: str) -> dict[str, dict]:
    return {
        f"{name}-id": {
            "id": f"{name}-id",
            "name": name,
            "version": "1.2.3",
            "workspace_dependencies": set(),
        }
        for name in names
    }


class PackageWorkspaceTest(unittest.TestCase):
    def test_unpublished_workspace_dependency_is_a_named_exception(self) -> None:
        # A crate that only fails because a workspace sibling is not on
        # crates.io yet is the one acceptable gap in a from-checkout dry run:
        # the publisher's layered ordering uploads that sibling first.
        package_workspace = load_package_workspace_module()
        failure = subprocess.CompletedProcess(
            ["cargo", "package"],
            101,
            "",
            "error: failed to prepare local package for uploading\n"
            "Caused by:\n"
            "  no matching package named `lash-internal-sansio` found\n",
        )
        ok = subprocess.CompletedProcess(["cargo", "package"], 0, "", "")

        with (
            mock.patch.object(
                package_workspace.subprocess, "run", side_effect=[ok, failure]
            ),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            packaged, exceptions, failures = package_workspace.package_individually(
                ["lash-internal-sansio", "lash-internal-typescript"],
                packages("lash-internal-sansio", "lash-internal-typescript"),
                no_verify=True,
            )

        self.assertEqual(packaged, ["lash-internal-sansio"])
        self.assertEqual(failures, {})
        self.assertIn("lash-internal-typescript", exceptions)
        self.assertIn("lash-internal-sansio", exceptions["lash-internal-typescript"])

    def test_packaging_defect_is_fatal(self) -> None:
        # A missing include or a bad manifest is exactly what this job exists
        # to catch, so it must never be filed as an ordering exception.
        package_workspace = load_package_workspace_module()
        failure = subprocess.CompletedProcess(
            ["cargo", "package"],
            101,
            "",
            "error: all dependencies must have a version requirement specified "
            "when packaging.\n",
        )

        with (
            mock.patch.object(package_workspace.subprocess, "run", return_value=failure),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            packaged, exceptions, failures = package_workspace.package_individually(
                ["lash-internal-sansio"], packages("lash-internal-sansio"), no_verify=True
            )

        self.assertEqual(packaged, [])
        self.assertEqual(exceptions, {})
        self.assertIn("lash-internal-sansio", failures)

    def test_missing_workspace_crate_is_not_mistaken_for_a_sibling(self) -> None:
        # Only a *workspace* crate missing from the index is an ordering
        # exception; a third-party dependency that does not resolve is a defect.
        package_workspace = load_package_workspace_module()
        failure = subprocess.CompletedProcess(
            ["cargo", "package"],
            101,
            "",
            "error: no matching package named `some-third-party-crate` found\n",
        )

        with (
            mock.patch.object(package_workspace.subprocess, "run", return_value=failure),
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            _, exceptions, failures = package_workspace.package_individually(
                ["lash-internal-sansio"], packages("lash-internal-sansio"), no_verify=True
            )

        self.assertEqual(exceptions, {})
        self.assertIn("lash-internal-sansio", failures)

    def test_verify_build_runs_unless_explicitly_skipped(self) -> None:
        package_workspace = load_package_workspace_module()
        ok = subprocess.CompletedProcess(["cargo", "package"], 0, "", "")

        with (
            mock.patch.object(package_workspace.subprocess, "run", return_value=ok) as run,
            contextlib.redirect_stdout(io.StringIO()),
            contextlib.redirect_stderr(io.StringIO()),
        ):
            package_workspace.run_cargo_package(["lash-internal-sansio"], no_verify=False)
            package_workspace.run_cargo_package(["lash-internal-sansio"], no_verify=True)

        self.assertNotIn("--no-verify", run.call_args_list[0].args[0])
        self.assertIn("--no-verify", run.call_args_list[1].args[0])

    def test_digests_are_read_from_the_packaged_crates(self) -> None:
        package_workspace = load_package_workspace_module()
        with tempfile.TemporaryDirectory() as directory:
            target = pathlib.Path(directory)
            (target / "package").mkdir()
            crate = target / "package" / "lash-internal-sansio-1.2.3.crate"
            crate.write_bytes(b"packaged bytes")

            digests = package_workspace.crate_digests(
                target, packages("lash-internal-sansio", "lash-internal-typescript")
            )

        self.assertEqual(
            digests,
            {"lash-internal-sansio": hashlib.sha256(b"packaged bytes").hexdigest()},
        )


if __name__ == "__main__":
    unittest.main()
