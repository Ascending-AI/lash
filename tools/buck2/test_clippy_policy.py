#!/usr/bin/env python3

from __future__ import annotations

import importlib.util
import json
import pathlib
import re
import tempfile
import textwrap
import tomllib
import unittest


ROOT = pathlib.Path(__file__).resolve().parents[2]
SPEC = importlib.util.spec_from_file_location(
    "clippy_policy", ROOT / "tools/buck2/clippy_policy.py"
)
assert SPEC and SPEC.loader
POLICY = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(POLICY)
OVERLAY_SPEC = importlib.util.spec_from_file_location(
    "prelude_overlay", ROOT / "tools/buck2/prelude_overlay.py"
)
assert OVERLAY_SPEC and OVERLAY_SPEC.loader
OVERLAY = importlib.util.module_from_spec(OVERLAY_SPEC)
OVERLAY_SPEC.loader.exec_module(OVERLAY)


def repository_canonical() -> dict:
    workspace = tomllib.loads((ROOT / "Cargo.toml").read_text(encoding="utf-8"))
    manifests = []
    for pattern in workspace["workspace"]["members"]:
        manifests.extend(ROOT.glob(pattern + "/Cargo.toml"))
    packages = [
        {
            "id": str(manifest.parent.relative_to(ROOT)),
            "manifest_path": str(manifest),
        }
        for manifest in sorted(set(manifests))
    ]
    return {
        "packages": packages,
        "workspace_members": [package["id"] for package in packages],
    }


class ClippyPolicyTests(unittest.TestCase):
    def test_repository_policy_covers_every_workspace_member(self) -> None:
        rendered = POLICY.render(repository_canonical(), ROOT)
        inventory = json.loads(
            (ROOT / "tools/buck2/target-inventory.json").read_text(encoding="utf-8")
        )
        self.assertIn(
            f"WORKSPACE_PACKAGE_COUNT = {len(inventory['packages'])}", rendered
        )
        self.assertIn('"--deny=unsafe_code"', rendered)
        self.assertIn('"--deny=clippy::unwrap_used"', rendered)
        self.assertIn('"--warn=clippy::large_futures"', rendered)
        self.assertIn('"crates/lash-core"', rendered)
        self.assertIn('"examples/agent-workbench"', rendered)
        self.assertIn('clippy_toml = spec[1]', rendered)
        self.assertNotIn("toml_merge_tool", rendered)
        provider = (ROOT / "tools/buck2/clippy_configuration.bzl").read_text(
            encoding="utf-8"
        )
        self.assertIn(
            'load("@prelude//rust:clippy_configuration.bzl", "ClippyConfiguration")',
            provider,
        )
        self.assertIn("ClippyConfiguration(clippy_toml = config)", provider)
        self.assertNotIn("ctx.actions", provider)

    def test_nearest_configuration_and_lint_levels_are_rendered(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "member/nested").mkdir(parents=True)
            (root / "clippy.toml").write_text("disallowed-methods = []\n")
            (root / "member/clippy.toml").write_text("disallowed-methods = []\n")
            (root / "Cargo.toml").write_text(
                textwrap.dedent(
                    """
                    [workspace.lints.rust]
                    unsafe_code = "deny"
                    unexpected_cfgs = { level = "deny", check-cfg = ["cfg(loom)"] }

                    [workspace.lints.clippy]
                    unwrap_used = "deny"
                    large_futures = "warn"
                    """
                )
            )
            manifest = root / "member/nested/Cargo.toml"
            manifest.write_text("[lints]\nworkspace = true\n")
            canonical = {
                "workspace_members": ["member"],
                "packages": [{"id": "member", "manifest_path": str(manifest)}],
            }
            rendered = POLICY.render(canonical, root)
            self.assertIn('"--check-cfg=cfg(loom)"', rendered)
            self.assertIn('"--deny=clippy::unwrap_used"', rendered)
            self.assertIn('"--warn=clippy::large_futures"', rendered)
            self.assertIn('"member": ("clippy_config_member", "//member:clippy.toml")', rendered)
            self.assertIn("clippy_toml = spec[1]", rendered)
            self.assertNotIn("toml_merge_tool", rendered)
            self.assertLess(rendered.index('"":'), rendered.index('"member":'))

    def test_member_without_workspace_lints_fails_closed(self) -> None:
        with tempfile.TemporaryDirectory() as directory:
            root = pathlib.Path(directory)
            (root / "member").mkdir()
            (root / "clippy.toml").write_text("disallowed-methods = []\n")
            (root / "Cargo.toml").write_text(
                "[workspace.lints.rust]\nunsafe_code = \"deny\"\n"
                "[workspace.lints.clippy]\nunwrap_used = \"deny\"\n"
            )
            manifest = root / "member/Cargo.toml"
            manifest.write_text("[lints]\nworkspace = false\n")
            canonical = {
                "workspace_members": ["member"],
                "packages": [{"id": "member", "manifest_path": str(manifest)}],
            }
            with self.assertRaisesRegex(ValueError, "does not inherit"):
                POLICY.render(canonical, root)

    def test_first_party_macros_apply_lints_configs_and_compile_inputs(self) -> None:
        source = (ROOT / "tools/buck2/lash_rust.bzl").read_text(encoding="utf-8")
        cargo_env = re.search(
            r"^def _cargo_env\([^\n]*\):\n(?P<body>.*?)(?=^def )",
            source,
            re.MULTILINE | re.DOTALL,
        )
        self.assertIsNotNone(cargo_env)
        namespace: dict[str, object] = {}
        exec(
            "def _cargo_env(package_name, crate_name, manifest_dir, "
            "version, extra = {}):\n" + cargo_env.group("body"),
            namespace,
        )
        environment = namespace["_cargo_env"]("package", "crate", "crates/package", "1.0")
        self.assertEqual(environment["CARGO_MANIFEST_DIR"], "crates/package")
        self.assertEqual(
            environment["KILN_RELATIVE_CARGO_MANIFEST_DIR"], "crates/package"
        )

        # Every Rust target is declared through `_rust_rule`, which declares
        # its Clippy twin with the same attributes.
        native_rules = re.findall(
            r"_rust_rule\(\n        (?:native\.rust_(?:binary|library|test)|lash_run_binary if run_attrs else native\.rust_binary),\n(?P<body>.*?)(?=^    \))",
            source,
            re.MULTILINE | re.DOTALL,
        )
        self.assertEqual(len(native_rules), 6)
        for rule in native_rules:
            self.assertIn(
                "clippy_configuration = first_party_clippy_configuration(manifest_dir)",
                rule,
            )
        self.assertIn(
            "FIRST_PARTY_RUST_LINT_FLAGS + FIRST_PARTY_CLIPPY_LINT_FLAGS",
            source,
        )
        self.assertEqual(
            source.count("resources = _resources(package_compile_data, extra_compile_data),"), 4
        )
        # Package compile data joins the sources; cross-package compile data
        # selects the repository-rooted source tree.
        self.assertEqual(
            source.count(
                " + package_compile_data,\n        extra_compile_data,\n    )\n"
            ),
            4,
        )
        self.assertIn(
            "_srcs(crate_root, srcs_patterns) + package_data,\n        extra_compile_data,\n    )\n",
            source,
        )
        self.assertIn(
            'package_files = glob(["**"], exclude = _IGNORED + data_exclude)',
            source,
        )
        self.assertIn(
            "resources = _resources(package_files, extra_compile_data + extra_data)", source
        )
        self.assertIn(
            '_srcs("build.rs", ["build/**/*.rs"]) + data,\n        extra_srcs,\n    )\n',
            source,
        )
        build_script = native_rules[0]
        self.assertIn('visibility = ["PUBLIC"]', build_script)

        overlay = (ROOT / "tools/buck2/prelude_overlay.py").read_text(encoding="utf-8")
        self.assertIn(
            'plain_env.pop("KILN_RELATIVE_CARGO_MANIFEST_DIR", None)', overlay
        )
        self.assertIn('path_env.pop("CARGO_MANIFEST_DIR", None)', overlay)
        self.assertIn(
            'plain_env["CARGO_MANIFEST_DIR"] = relative_manifest_dir', overlay
        )

        stock_process_env_tail = """    for key in _DIRECTORY_ENV:
        value = plain_env.pop(key, None)
        if value:
            path_env[key] = value

    return (plain_env, path_env)
"""
        transformed = OVERLAY.preserve_relative_manifest_dir(stock_process_env_tail)
        self.assertNotIn(
            "KILN_RELATIVE_CARGO_MANIFEST_DIR", stock_process_env_tail
        )
        self.assertIn(
            'relative_manifest_dir = plain_env.pop("KILN_RELATIVE_CARGO_MANIFEST_DIR", None)',
            transformed,
        )
        self.assertEqual(
            OVERLAY.preserve_relative_manifest_dir(transformed), transformed
        )


if __name__ == "__main__":
    unittest.main()
