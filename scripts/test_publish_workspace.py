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
import tomllib
import unittest
from unittest import mock


ROOT = pathlib.Path(__file__).resolve().parent.parent

EXPECTED_INTERNAL_PACKAGES = {
    "lash-core": "lash-internal-core",
    "lash-core-effect": "lash-internal-core-effect",
    "lash-core-execution": "lash-internal-core-execution",
    "lash-core-ids": "lash-internal-core-ids",
    "lash-core-llm": "lash-internal-core-llm",
    "lash-core-memory": "lash-internal-core-memory",
    "lash-core-store": "lash-internal-core-store",
    "lash-core-worker": "lash-internal-core-worker",
    "lash-http-transport": "lash-internal-http-transport",
    "lash-lashlang-runtime": "lash-internal-lashlang-runtime",
    "lash-llm-tools": "lash-internal-llm-tools",
    "lash-llm-transport": "lash-internal-llm-transport",
    "lash-plugin-mcp": "lash-internal-plugin-mcp",
    "lash-plugin-process-controls": "lash-internal-plugin-process-controls",
    "lash-plugin-tool-output-budget": "lash-internal-plugin-tool-output-budget",
    "lash-plugin-rolling-history": "lash-internal-plugin-rolling-history",
    "lash-postgres-store": "lash-internal-postgres-store",
    "lash-protocol-rlm": "lash-internal-protocol-rlm",
    "lash-protocol-standard": "lash-internal-protocol-standard",
    "lash-provider-anthropic": "lash-internal-provider-anthropic",
    "lash-provider-auth": "lash-internal-provider-auth",
    "lash-provider-google": "lash-internal-provider-google",
    "lash-provider-openai": "lash-internal-provider-openai",
    "lash-remote-protocol": "lash-internal-remote-protocol",
    "lash-restate": "lash-internal-restate",
    "lash-rlm-types": "lash-internal-rlm-types",
    "lash-s3-store": "lash-internal-s3-store",
    "lash-sansio": "lash-internal-sansio",
    "lash-sqlite-store": "lash-internal-sqlite-store",
    "lash-store-sql": "lash-internal-store-sql",
    "lash-subagents": "lash-internal-subagents",
    "lash-tool-support": "lash-internal-tool-support",
    "lash-trace": "lash-internal-trace",
    "lash-typescript": "lash-internal-typescript",
    "lashlang": "lash-internal-lashlang",
}


def load_publish_workspace_module():
    module_path = ROOT / "scripts" / "publish_workspace.py"
    spec = importlib.util.spec_from_file_location("publish_workspace", module_path)
    if spec is None or spec.loader is None:
        raise RuntimeError(f"failed to load {module_path}")
    module = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(module)
    return module


def build_internal_dependency_closure(
    metadata: dict, workspace_members: set[str]
) -> dict[str, set[str]]:
    """Package id -> every workspace package it reaches through non-dev deps."""
    id_by_dir = {
        pathlib.Path(package["manifest_path"]).parent: package["id"]
        for package in metadata["packages"]
        if package["id"] in workspace_members
    }
    edges: dict[str, set[str]] = {}
    for package in metadata["packages"]:
        if package["id"] not in workspace_members:
            continue
        edges[package["id"]] = {
            id_by_dir[pathlib.Path(dependency["path"])]
            for dependency in package["dependencies"]
            if dependency.get("kind") != "dev"
            and dependency.get("path")
            and pathlib.Path(dependency["path"]) in id_by_dir
        }

    closure: dict[str, set[str]] = {}
    for package_id in edges:
        reached: set[str] = set()
        pending = list(edges[package_id])
        while pending:
            current = pending.pop()
            if current in reached:
                continue
            reached.add(current)
            pending.extend(edges.get(current, ()))
        closure[package_id] = reached
    return closure


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

    def test_stamp_workspace_invokes_release_version_stamp(self) -> None:
        # --version stamps the real release version into the tree being
        # published, by running the workspace checkout's own release_version.py.
        publish_workspace = load_publish_workspace_module()
        with (
            mock.patch.object(
                publish_workspace, "run_json", return_value={"workspace_root": str(ROOT)}
            ),
            mock.patch.object(publish_workspace.subprocess, "run") as run,
        ):
            publish_workspace.stamp_workspace("1.2.3")
        run.assert_called_once()
        stamp_args = run.call_args[0][0]
        self.assertIn("stamp", stamp_args)
        self.assertIn("1.2.3", stamp_args)
        self.assertTrue(
            any(str(arg).endswith("release_version.py") for arg in stamp_args),
            stamp_args,
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

    def test_lash_regress_publishes_before_lashlang(self) -> None:
        publish_workspace = load_publish_workspace_module()
        packages = {
            "lash-regress": {
                "id": "lash-regress",
                "name": "lash-regress",
                "version": "1",
                "workspace_dependencies": set(),
            },
            "lashlang": {
                "id": "lashlang",
                "name": "lash-internal-lashlang",
                "version": "1",
                "workspace_dependencies": {"lash-regress"},
            },
        }
        layers = publish_workspace.compute_layers(packages)
        self.assertEqual(layers, [["lash-regress"], ["lashlang"]])

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
        runtime_dir = str(ROOT / "crates" / "lash-lashlang-runtime")
        store_dir = str(ROOT / "crates" / "lash-postgres-store")
        metadata = {
            "workspace_members": ["runtime-id", "store-id"],
            "packages": [
                {
                    "id": "runtime-id",
                    "name": "lash-internal-lashlang-runtime",
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
                            "name": "lash-internal-lashlang-runtime",
                            "rename": "lash-lashlang-runtime",
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
        layers = publish_workspace.compute_layers(packages)
        self.assertEqual(layers, [["runtime-id"], ["store-id"]])

    def test_versioned_dev_dependencies_gate_publish_ordering(self) -> None:
        # A workspace-versioned dev-dependency survives into the published
        # manifest and must resolve on the index when cargo packages the
        # crate (this partially failed the v0.1.0-alpha.81 publish when
        # lash-postgres-store's conformance dev-dep on lash-lashlang-runtime
        # was packaged before that crate was visible). Versioned dev-deps are
        # ordering edges; version-less path dev-deps stay excluded.
        publish_workspace = load_publish_workspace_module()
        runtime_dir = str(ROOT / "crates" / "lash-lashlang-runtime")
        store_dir = str(ROOT / "crates" / "lash-postgres-store")
        metadata = {
            "workspace_members": ["runtime-id", "store-id"],
            "packages": [
                {
                    "id": "runtime-id",
                    "name": "lash-internal-lashlang-runtime",
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
                            "name": "lash-internal-lashlang-runtime",
                            "rename": "lash-lashlang-runtime",
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

    def test_workspace_distribution_contract_is_congruent(self) -> None:
        publish_workspace = load_publish_workspace_module()
        metadata = publish_workspace.run_json(
            ["cargo", "metadata", "--format-version", "1", "--locked", "--no-deps"]
        )
        workspace_members = set(metadata["workspace_members"])
        publishable = {
            package["name"]
            for package in metadata["packages"]
            if package["id"] in workspace_members and package.get("publish") != []
        }
        self.assertEqual(
            publishable,
            set(EXPECTED_INTERNAL_PACKAGES.values()) | {"lash-runtime", "lash-regress"},
        )

        root_manifest = tomllib.loads((ROOT / "Cargo.toml").read_text())
        workspace_dependencies = root_manifest["workspace"]["dependencies"]
        for alias, package_name in EXPECTED_INTERNAL_PACKAGES.items():
            with self.subTest(alias=alias):
                dependency = workspace_dependencies[alias]
                self.assertEqual(dependency["package"], package_name)
                self.assertEqual(dependency["version"], "=0.0.0-dev")
                self.assertIn("path", dependency)

        # Internal deps pin the exact workspace version so a published crate
        # never resolves a sibling from another release. The one exception is a
        # dev-dependency pointing back at a crate that already depends on this
        # one: a versioned dev-dep survives into the published manifest and has
        # to resolve on the index, so pinning it would make the pair
        # unpublishable (and unpackageable) in either order. Version-less path
        # dev-deps are stripped when packaging, which is what breaks the cycle.
        depends_on = build_internal_dependency_closure(metadata, workspace_members)
        for package in metadata["packages"]:
            if package["id"] not in workspace_members or package.get("publish") == []:
                continue
            for dependency in package["dependencies"]:
                if dependency["name"] not in EXPECTED_INTERNAL_PACKAGES.values():
                    continue
                target = next(
                    candidate
                    for candidate in metadata["packages"]
                    if pathlib.Path(candidate["manifest_path"]).parent
                    == pathlib.Path(dependency["path"])
                )
                if target["id"] == package["id"]:
                    continue
                cyclic_dev_dependency = (
                    dependency["kind"] == "dev"
                    and package["id"] in depends_on[target["id"]]
                )
                self.assertEqual(
                    dependency["req"],
                    "*" if cyclic_dev_dependency else "=0.0.0-dev",
                    f"{package['name']} -> {dependency['name']}",
                )

        facade = next(
            package
            for package in metadata["packages"]
            if package["name"] == "lash-runtime"
        )
        remote = next(
            dependency
            for dependency in facade["dependencies"]
            if dependency["name"] == "lash-internal-remote-protocol"
            and dependency["kind"] is None
        )
        self.assertEqual(remote["rename"], "lash-remote-protocol")
        self.assertIn("core-conversions", remote["features"])

        for extension_name in (
            "lash-internal-postgres-store",
            "lash-internal-provider-openai",
            "lash-internal-restate",
        ):
            extension = next(
                package
                for package in metadata["packages"]
                if package["name"] == extension_name
            )
            facade_dev_dependency = next(
                dependency
                for dependency in extension["dependencies"]
                if dependency["name"] == "lash-runtime" and dependency["kind"] == "dev"
            )
            self.assertEqual(facade_dev_dependency["rename"], "lash")
            self.assertEqual(facade_dev_dependency["req"], "*")

    def test_uploaded_crate_digest_mismatch_fails_the_release(self) -> None:
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

    def test_uploaded_crate_digest_match_passes(self) -> None:
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

    def test_successful_publish_checks_the_uploaded_crate(self) -> None:
        publish_workspace = load_publish_workspace_module()
        args = argparse.Namespace(
            publish_timeout_seconds=600,
            publish_attempts=1,
            retry_delay_seconds=0,
            visibility_timeout_seconds=1,
            visibility_delay_seconds=0,
            upload_digest_attempts=1,
        )
        package = {"name": "lash-internal-sansio", "version": "1.2.3"}
        published = subprocess.CompletedProcess(["cargo", "publish"], 0, "", "")

        with (
            mock.patch.object(publish_workspace, "crate_version_visible", return_value=False),
            mock.patch.object(publish_workspace, "wait_for_crate_version"),
            mock.patch.object(publish_workspace, "verify_uploaded_crate") as verify,
            mock.patch.object(publish_workspace.subprocess, "run", return_value=published),
        ):
            with contextlib.redirect_stdout(io.StringIO()), contextlib.redirect_stderr(io.StringIO()):
                publish_workspace.publish_package(package, args)

        verify.assert_called_once_with(package["name"], package["version"], args)


if __name__ == "__main__":
    unittest.main()
