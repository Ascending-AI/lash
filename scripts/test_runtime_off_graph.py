#!/usr/bin/env python3
"""Exercise isolated Cargo manifest inheritance and patched resolver reachability."""

from __future__ import annotations

from pathlib import Path
import sys
import tempfile
import tomllib
from types import SimpleNamespace
import unittest

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools/bazel"))
import runtime_off_workspace as workspace

RESOLVER = None
if "--resolver" in sys.argv:
    index = sys.argv.index("--resolver")
    path = Path(sys.argv[index + 1])
    del sys.argv[index : index + 2]
    RESOLVER = {}
    # The upstream resolver's pure functions use the shared Python/Starlark
    # subset. Load those exact patched functions; these cases need no cfg
    # parser because their dependency platform sets are already resolved.
    exec("\n".join(path.read_text().splitlines()[1:]), RESOLVER)


class ManifestInheritance(unittest.TestCase):
    def test_inheritance_preserves_optional_edges_and_adds_dependency_features(self):
        with tempfile.TemporaryDirectory() as raw:
            root = Path(raw) / "repo"
            package = root / "crates/lash"
            (package / "src").mkdir(parents=True)
            (package / "src/lib.rs").write_text("pub struct Witness;\n")
            (root / "Cargo.toml").write_text("""[workspace]
members = ["crates/lash"]
resolver = "3"
[workspace.package]
version = "0.0.0"
edition = "2024"
[workspace.dependencies]
serde = { version = "1", default-features = false, features = ["derive"] }
""")
            (package / "Cargo.toml").write_text("""[package]
name = "lash-runtime"
version.workspace = true
edition.workspace = true
[dependencies]
serde = { workspace = true, optional = true, features = ["rc"] }
[features]
default = []
serialize = ["dep:serde"]
[dev-dependencies]
serde = { version = "1", features = ["std"] }
""")
            lock = root / "tools/bazel/runtime-off.Cargo.lock"
            lock.parent.mkdir(parents=True)
            lock.write_text("version = 4\n")
            output = Path(raw) / "output"
            workspace.materialize(root, output)
            result = tomllib.loads((output / "crates/lash/Cargo.toml").read_text())
            self.assertEqual(
                result["dependencies"]["serde"],
                {
                    "version": "1",
                    "default-features": False,
                    "optional": True,
                    "features": ["derive", "rc"],
                },
            )
            self.assertEqual(result["features"]["serialize"], ["dep:serde"])
            self.assertNotIn("dev-dependencies", result)
            self.assertEqual(result["package"]["version"], "0.0.0")
            witness = tomllib.loads((output / "witness/Cargo.toml").read_text())
            self.assertFalse(witness["dependencies"]["lash"]["default-features"])
            (package / "src/lib.rs").write_text('compile_error!("changed");\n')
            self.assertEqual(
                (output / "crates/lash/src/lib.rs").read_text(),
                'compile_error!("changed");\n',
            )


@unittest.skipUnless(
    RESOLVER is not None, "pass --resolver to exercise the fetched patched resolver"
)
class ResolverReachability(unittest.TestCase):
    def state(self, index):
        return SimpleNamespace(
            package_index=index,
            active_triples=set(),
            features_enabled={t: set() for t in ("linux", "windows")},
            deps={t: set() for t in ("linux", "windows")},
            build_deps={t: set() for t in ("linux", "windows")},
            aliases={},
            possible_features={},
            possible_deps=[],
        )

    def package(self, name, state):
        return {"name": name, "version": "1", "feature_resolutions": state}

    def dependency(self, name, state):
        return {
            "name": name,
            "bazel_target": "//:" + name,
            "target": {"linux", "windows"},
            "feature_resolutions": state,
        }

    def test_unreachable_package_does_not_enable_its_dependencies(self):
        root, unreachable, shared = (self.state(index) for index in range(3))
        root.active_triples.add("linux")
        root.possible_deps = [self.dependency("shared", shared)]
        unreachable.possible_deps = [
            self.dependency("shared", shared) | {"features": ["extra"]}
        ]
        states = {"root": root, "unreachable": unreachable, "shared": shared}
        packages = [self.package(name, state) for name, state in states.items()]
        RESOLVER["resolve"](None, packages, states, {}, False)
        self.assertEqual(shared.active_triples, {"linux"})
        self.assertEqual(shared.features_enabled["linux"], set())
        self.assertEqual(unreachable.active_triples, set())

    def test_later_subfeature_keeps_original_platform_applicability(self):
        root, shared = self.state(0), self.state(1)
        root.active_triples.add("linux")
        root.possible_features = {"later": ["shared/extra"]}
        root.possible_deps = [self.dependency("shared", shared)]
        packages = [self.package("root", root), self.package("shared", shared)]
        RESOLVER["_resolve_one_round"](packages, [0], {}, False)
        root.features_enabled["linux"].add("later")
        RESOLVER["_resolve_one_round"](packages, [0], {}, False)
        self.assertEqual(shared.features_enabled["linux"], {"extra"})
        self.assertEqual(shared.active_triples, {"linux"})
        self.assertEqual(root.possible_deps[0]["target"], {"linux", "windows"})


if __name__ == "__main__":
    unittest.main()
