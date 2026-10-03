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
