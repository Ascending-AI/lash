#!/usr/bin/env python3
"""The synthetic dependency graph retains exact git package identities."""

import hashlib
import json
import pathlib
import tempfile
import tomllib
import unittest
from unittest.mock import patch

import sync


class GitSources(unittest.TestCase):
    def manifest(self, sources):
        packages = [
            {"id": f"dependency-{index}", "name": name, "version": "1.2.3", "source": source}
            for index, (name, source) in enumerate(sources)
        ]
        canonical = {
            "workspace_members": [],
            "packages": packages,
            "resolve": {
                "nodes": [{"id": package["id"], "features": ["enabled"]} for package in packages]
            },
        }
        with patch.object(sync, "EXTRA_LOCKED_CRATES", {}):
            return sync.synthetic_manifest(canonical)

    def test_registry_manifest_is_unchanged(self):
        self.assertEqual(
            self.manifest([("ordinary", "registry+https://github.com/rust-lang/crates.io-index")]),
            sync.GENERATED_HEADER
            + '[package]\nname = "lash-buck2-third-party"\nversion = "0.0.0"\n'
            + 'edition = "2024"\npublish = false\n\n[workspace]\nresolver = "2"\n\n'
            + '[dependencies.p0000]\npackage = "ordinary"\nversion = "=1.2.3"\n'
            + 'default-features = false\nfeatures = ["enabled"]\n',
        )

    def test_path_patch_keeps_identity_features_and_stays_outside_workspace(self):
        package = {
            "id": "path+rmcp", "name": "rmcp", "version": "3.5.1", "source": None,
            "manifest_path": str(sync.SYNTHETIC / "patched/rmcp/Cargo.toml"),
        }
        canonical = {
            "workspace_members": [], "packages": [package],
            "resolve": {"nodes": [{"id": package["id"], "features": ["client", "server"]}]},
        }
        with patch.object(sync, "EXTRA_LOCKED_CRATES", {}):
            manifest = tomllib.loads(sync.synthetic_manifest(canonical))
        self.assertEqual(manifest["workspace"]["exclude"], ["patched/rmcp"])
        self.assertEqual(manifest["dependencies"]["p0000"], {
            "package": "rmcp", "version": "=3.5.1", "path": "patched/rmcp",
            "default-features": False, "features": ["client", "server"],
        })
        package["manifest_path"] = str(sync.SYNTHETIC / "unreviewed/rmcp/Cargo.toml")
        with self.assertRaisesRegex(ValueError, "reviewed patch"):
            sync.synthetic_manifest(canonical)

    def test_path_patch_projects_sources_and_buildscript_in_archive_layout(self):
        prefix = "third-party/patched/example"
        package = {"name": "example", "version": "1.2.3", "source": None,
                   "manifest_path": str(sync.ROOT / prefix / "Cargo.toml")}
        library = (
            'third_party_rust_library(\n    name = "example-1",\n'
            f'    srcs = ["{prefix}/src/lib.rs"],\n'
            f'    crate_root = "{prefix}/src/lib.rs",\n'
            f'    env = {{"CARGO_PKG_NAME": "example", "CARGO_MANIFEST_DIR": "{prefix}"}},\n)\n'
        )
        binary = library.replace("third_party_rust_library", "third_party_rust_binary").replace("src/lib.rs", "build.rs")
        unrelated = 'third_party_rust_library(\n    name = "other-1",\n)\n'
        buildscript = (
            'third_party_buildscript_run(\n    package_name = "example",\n'
            f'    local_manifest_dir = "{prefix}",\n)\n'
        )
        rendered, outputs = sync.patched_path_rules(library + binary + buildscript + unrelated, {"packages": [package]})
        self.assertEqual(rendered.count('srcs = ["//third-party/patched/example:crate_sources"]'), 2)
        self.assertIn('crate_root = "example-1.2.3.crate/src/lib.rs"', rendered)
        self.assertIn('crate_root = "example-1.2.3.crate/build.rs"', rendered)
        self.assertIn('"CARGO_MANIFEST_DIR": "example-1.2.3.crate"', rendered)
        self.assertIn('manifest_dir = "//third-party/patched/example:crate_sources"', rendered)
        self.assertNotIn("local_manifest_dir", rendered)
        self.assertIn(unrelated, rendered)
        sources = outputs[sync.ROOT / prefix / "BUCK"]
        self.assertIn('out = "example-1.2.3.crate"', sources)
        self.assertIn('glob(["**"]', sources)
        self.assertNotIn("http_archive", rendered)

    def test_git_packages_keep_the_repository_and_full_revision(self):
        revision = "0123456789abcdef0123456789abcdef01234567"
        repository = "https://example.com/owner/library"
        source = f"git+{repository}?rev={revision}#{revision}"
        dependencies = tomllib.loads(self.manifest([("library", source), ("macros", source)]))[
            "dependencies"
        ]
        self.assertEqual(len(dependencies), 2)
        for dependency in dependencies.values():
            self.assertEqual(dependency["git"], repository)
            self.assertEqual(dependency["rev"], revision)
            self.assertEqual(dependency["version"], "=1.2.3")
            self.assertEqual(dependency["features"], ["enabled"])
            self.assertFalse(dependency["default-features"])

    def test_git_source_without_a_full_resolved_revision_is_refused(self):
        for source in ["git+https://example.com/library?rev=main", "git+https://example.com/library#1234"]:
            with self.subTest(source=source), self.assertRaisesRegex(ValueError, "resolved git revision"):
                self.manifest([("library", source)])

    def test_git_fetch_uses_vendored_root_and_nested_packages(self):
        registry = 'third_party_http_archive(\n    name = "registry.crate",\n)\n'
        content = 'git_fetch(\n    name = "library.git",\n)\n' + registry
        packages = []
        for name, directory in [("library", ""), ("macros", "macros/")]:
            content += (
                'third_party_rust_library(\n    srcs = [":library.git"],\n'
                f'    crate_root = "library/{directory}src/lib.rs",\n'
                f'    env = {{"CARGO_PKG_NAME": "{name}", "CARGO_PKG_VERSION": "1.2.3"}},\n)\n'
            )
            packages.append({
                "name": name, "version": "1.2.3", "source": "git+https://example.com/library#" + "a" * 40,
                "manifest_path": f"/checkout/{directory}Cargo.toml",
                "targets": [{"src_path": f"/checkout/{directory}src/lib.rs"}],
            })
        rendered, recipe = sync.vendored_git_rules(content, {"packages": packages})
        self.assertNotIn("git_fetch(", rendered)
        self.assertIn('filegroup(\n    name = "library.git",', rendered)
        self.assertIn('out = "library"', rendered)
        self.assertIn(registry, rendered)
        self.assertEqual(recipe["sources"][0]["root"], "library")
        self.assertEqual(
            [(member["directory"], member["vendor"]) for member in recipe["sources"][0]["packages"]],
            [("", "library-1.2.3"), ("macros", "macros-1.2.3")],
        )

    def test_vendored_sources_materialize_and_repair_from_pinned_checksums(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = pathlib.Path(temporary)
            vendor = root / "vendor/library-1.2.3"
            vendor.mkdir(parents=True)
            (vendor / "lib.rs").write_text("pub fn pinned() {}\n")
            (vendor / ".cargo-checksum.json").write_text(json.dumps({
                "files": {"lib.rs": hashlib.sha256((vendor / "lib.rs").read_bytes()).hexdigest()},
            }))
            synthetic = root / "third-party"
            (synthetic / "rust").mkdir(parents=True)
            (synthetic / "rust/git-sources.json").write_text(json.dumps({"sources": [{
                "root": "library", "packages": [{"directory": "", "vendor": "library-1.2.3"}],
            }]}))
            with patch.object(sync, "ROOT", root), patch.object(sync, "SYNTHETIC", synthetic):
                sync.materialize_git_sources()
                material = synthetic / "rust/.git-sources/library/lib.rs"
                self.assertEqual(material.read_bytes(), (vendor / "lib.rs").read_bytes())
                material.write_text("changed\n")
                sync.materialize_git_sources()
                self.assertEqual(material.read_bytes(), (vendor / "lib.rs").read_bytes())
                (vendor / "lib.rs").write_text("wrong revision\n")
                material.unlink()
                with self.assertRaisesRegex(ValueError, "Cargo checksums"):
                    sync.materialize_git_sources()


if __name__ == "__main__":
    unittest.main()
