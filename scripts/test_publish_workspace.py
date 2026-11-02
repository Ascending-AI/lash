#!/usr/bin/env python3
from __future__ import annotations

import argparse
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

def load_publish_workspace_module():
    module_path = ROOT / "scripts" / "publish_workspace.py"
    spec = importlib.util.spec_from_file_location("publish_workspace", module_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


class PublishWorkspaceTest(unittest.TestCase):
    def test_publish_retries_cargo_registry_http2_failures(self) -> None:
        publish_workspace = load_publish_workspace_module()
        args = argparse.Namespace(
            publish_timeout_seconds=600,
            publish_attempts=2,
            retry_delay_seconds=0,
            visibility_timeout_seconds=1,
            visibility_delay_seconds=0,
        )
        package = {
            "name": "lash-internal-provider-anthropic",
            "version": "0.1.0-alpha.40",
        }
        first = subprocess.CompletedProcess(
            ["cargo", "publish"],
            101,
            "",
            "\n".join(
                [
                    "error: download of config.json failed",
                    "Caused by:",
                    "  curl failed",
                    "Caused by:",
                    "  [16] Error in the HTTP2 framing layer",
                ]
            ),
        )
        second = subprocess.CompletedProcess(["cargo", "publish"], 0, "", "")

        with (
            mock.patch.object(publish_workspace, "crate_version_visible", return_value=False),
            mock.patch.object(publish_workspace, "wait_for_crate_version") as wait,
            mock.patch.object(publish_workspace, "verify_uploaded_crate"),
            mock.patch.object(publish_workspace.time, "sleep"),
            mock.patch.object(publish_workspace.subprocess, "run", side_effect=[first, second]) as run,
        ):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                publish_workspace.publish_package(package, args)

        self.assertEqual(run.call_count, 2)
        wait.assert_called_once_with(package["name"], package["version"], args)

    def test_dev_dependencies_do_not_gate_publish_ordering(self) -> None:
        # A crate that dev-depends on itself (to enable one of its own
        # features for integration tests) and a provider that dev-depends
        # back onto the transport's test support must not deadlock the
        # publish planner: dev-deps are stripped from published packages.
        publish_workspace = load_publish_workspace_module()
        transport_dir = str(ROOT / "crates" / "lash-llm-transport")
        provider_dir = str(ROOT / "crates" / "lash-provider-anthropic")
        metadata = {
            "workspace_members": ["transport-id", "provider-id"],
            "packages": [
                {
                    "id": "transport-id",
                    "name": "lash-internal-llm-transport",
                    "version": "0.0.1",
                    "publish": None,
                    "manifest_path": f"{transport_dir}/Cargo.toml",
                    "dependencies": [
                        {
                            "name": "lash-internal-llm-transport",
                            "rename": "lash-llm-transport",
                            "path": transport_dir,
                            "kind": "dev",
                        },
                    ],
                },
                {
                    "id": "provider-id",
                    "name": "lash-internal-provider-anthropic",
                    "version": "0.0.1",
                    "publish": None,
                    "manifest_path": f"{provider_dir}/Cargo.toml",
                    "dependencies": [
                        {
                            "name": "lash-internal-llm-transport",
                            "rename": "lash-llm-transport",
                            "path": transport_dir,
                            "kind": None,
                            "req": "=0.0.1",
                        },
                        {
                            "name": "lash-internal-llm-transport",
                            "rename": "lash-llm-transport",
                            "path": transport_dir,
                            "kind": "dev",
                        },
                    ],
                },
            ],
        }
        with mock.patch.object(publish_workspace, "run_json", return_value=metadata):
            packages = publish_workspace.load_publishable_workspace_packages()

        self.assertEqual(packages["transport-id"]["workspace_dependencies"], set())
        self.assertEqual(
            packages["provider-id"]["workspace_dependencies"], {"transport-id"}
        )

    def test_compute_layers_orders_by_dependency_depth(self) -> None:
        # A chain leaf <- mid <- top publishes leaf-first, one crate per layer.
        publish_workspace = load_publish_workspace_module()
        packages = {
            "top": {"id": "top", "name": "top", "version": "1", "workspace_dependencies": {"mid"}},
            "mid": {"id": "mid", "name": "mid", "version": "1", "workspace_dependencies": {"leaf"}},
            "leaf": {"id": "leaf", "name": "leaf", "version": "1", "workspace_dependencies": set()},
        }
        layers = publish_workspace.compute_layers(packages)
        self.assertEqual(layers, [["leaf"], ["mid"], ["top"]])

    def test_compute_layers_groups_independent_crates_in_one_layer(self) -> None:
        # Two leaves plus a crate depending on both: leaves batch together, then
        # the dependent forms the next layer. In-layer order is by crate name.
        publish_workspace = load_publish_workspace_module()
        packages = {
            "b": {"id": "b", "name": "b-leaf", "version": "1", "workspace_dependencies": set()},
            "a": {"id": "a", "name": "a-leaf", "version": "1", "workspace_dependencies": set()},
            "top": {
                "id": "top",
                "name": "top",
                "version": "1",
                "workspace_dependencies": {"a", "b"},
            },
        }
        layers = publish_workspace.compute_layers(packages)
        self.assertEqual(layers, [["a", "b"], ["top"]])

    def test_compute_layers_skips_already_completed_crates(self) -> None:
        # A resumed run seeds the already-visible crate as completed, so the
        # dependent lands in the first computed layer.
        publish_workspace = load_publish_workspace_module()
        packages = {
            "leaf": {"id": "leaf", "name": "leaf", "version": "1", "workspace_dependencies": set()},
            "top": {"id": "top", "name": "top", "version": "1", "workspace_dependencies": {"leaf"}},
        }
        layers = publish_workspace.compute_layers(packages, {"leaf"})
        self.assertEqual(layers, [["top"]])

    def test_compute_layers_reports_dependency_cycle(self) -> None:
        publish_workspace = load_publish_workspace_module()
        packages = {
            "a": {"id": "a", "name": "a", "version": "1", "workspace_dependencies": {"b"}},
            "b": {"id": "b", "name": "b", "version": "1", "workspace_dependencies": {"a"}},
        }
        with self.assertRaises(RuntimeError):
            publish_workspace.compute_layers(packages)

    def test_deliberately_misordered_graph_is_rejected(self) -> None:
        publish_workspace = load_publish_workspace_module()
        packages = {
            "leaf": {
                "id": "leaf",
                "name": "lash-internal-leaf",
                "version": "1",
                "workspace_dependencies": set(),
            },
            "top": {
                "id": "top",
                "name": "lash-internal-top",
                "version": "1",
                "workspace_dependencies": {"leaf"},
            },
        }
        with self.assertRaisesRegex(RuntimeError, "places lash-internal-top before"):
            publish_workspace.validate_publish_order(packages, ["top", "leaf"])

    def test_versioned_dev_dependency_is_a_layering_edge(self) -> None:
        # The same versioned-dev-dep ordering constraint that gates the
        # one-at-a-time planner must gate the layered planner: the store
        # publishes in a later layer than the runtime it dev-depends on.
        publish_workspace = load_publish_workspace_module()
        runtime_dir = str(ROOT / "crates" / "lash-vm-runtime")
        store_dir = str(ROOT / "crates" / "lash-postgres-store")
        metadata = {
            "workspace_members": ["runtime-id", "store-id"],
            "packages": [
                {
                    "id": "runtime-id",
                    "name": "lash-internal-vm-runtime",
                    "version": "0.0.1",
                    "publish": None,
                    "manifest_path": f"{runtime_dir}/Cargo.toml",
                    "dependencies": [],
                },
                {
                    "id": "store-id",
                    "name": "lash-internal-postgres-store",
                    "version": "0.0.1",
                    "publish": None,
                    "manifest_path": f"{store_dir}/Cargo.toml",
                    "dependencies": [
                        {
                            "name": "lash-internal-vm-runtime",
                            "rename": "lash-vm-runtime",
                            "path": runtime_dir,
                            "kind": "dev",
                            "req": "=0.0.1",
                        },
                    ],
                },
            ],
        }
        with mock.patch.object(publish_workspace, "run_json", return_value=metadata):
            packages = publish_workspace.load_publishable_workspace_packages()
        self.assertEqual(
            packages["store-id"]["workspace_dependencies"], {"runtime-id"}
        )
        layers = publish_workspace.compute_layers(packages)
        self.assertEqual(layers, [["runtime-id"], ["store-id"]])

    def test_uploaded_crate_digest_mismatch_fails_the_release(self) -> None:
        publish_workspace = load_publish_workspace_module()
        args = argparse.Namespace(upload_digest_attempts=1, retry_delay_seconds=0)
        payload = b"identical bytes"
        checksum = hashlib.sha256(payload).hexdigest()
        with tempfile.TemporaryDirectory() as directory:
            target = pathlib.Path(directory)
            (target / "package").mkdir()
            (target / "package" / "lash-internal-sansio-1.2.3.crate").write_bytes(payload)

            with (
                mock.patch.object(publish_workspace, "target_directory", return_value=target),
                mock.patch.object(
                    publish_workspace, "download_uploaded_crate", return_value=payload
                ),
                mock.patch.object(
                    publish_workspace, "registry_crate_checksum", return_value=checksum
                ),
            ):
                with contextlib.redirect_stdout(io.StringIO()):
                    publish_workspace.verify_uploaded_crate("lash-internal-sansio", "1.2.3", args)

        # The point of the post-publish check: if crates.io serves anything
        # other than the bytes this job packaged, the release stops.
        publish_workspace = load_publish_workspace_module()
        args = argparse.Namespace(upload_digest_attempts=1, retry_delay_seconds=0)
        with tempfile.TemporaryDirectory() as directory:
            target = pathlib.Path(directory)
            (target / "package").mkdir()
            (target / "package" / "lash-internal-sansio-1.2.3.crate").write_bytes(b"local")

            with (
                mock.patch.object(publish_workspace, "target_directory", return_value=target),
                mock.patch.object(
                    publish_workspace, "download_uploaded_crate", return_value=b"other"
                ),
                mock.patch.object(publish_workspace, "registry_crate_checksum", return_value=None),
            ):
                with contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaises(RuntimeError) as raised:
                        publish_workspace.verify_uploaded_crate(
                            "lash-internal-sansio", "1.2.3", args
                        )

        self.assertIn("UPLOADED CRATE DIGEST MISMATCH", str(raised.exception))

    def test_registry_recorded_checksum_mismatch_fails_the_release(self) -> None:
        publish_workspace = load_publish_workspace_module()
        args = argparse.Namespace(upload_digest_attempts=1, retry_delay_seconds=0)
        payload = b"identical bytes"
        with tempfile.TemporaryDirectory() as directory:
            target = pathlib.Path(directory)
            (target / "package").mkdir()
            (target / "package" / "lash-internal-sansio-1.2.3.crate").write_bytes(payload)

            with (
                mock.patch.object(publish_workspace, "target_directory", return_value=target),
                mock.patch.object(
                    publish_workspace, "download_uploaded_crate", return_value=payload
                ),
                mock.patch.object(
                    publish_workspace, "registry_crate_checksum", return_value="0" * 64
                ),
            ):
                with contextlib.redirect_stdout(io.StringIO()):
                    with self.assertRaises(RuntimeError) as raised:
                        publish_workspace.verify_uploaded_crate(
                            "lash-internal-sansio", "1.2.3", args
                        )

        self.assertIn("UPLOADED CRATE DIGEST MISMATCH", str(raised.exception))


if __name__ == "__main__":
    unittest.main()
