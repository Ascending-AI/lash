from __future__ import annotations

import importlib.util
import io
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import unittest
import unittest.mock


SCRIPT = Path(__file__).with_name("rustdoc_json_cache.py")
SPEC = importlib.util.spec_from_file_location("rustdoc_json_cache", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
MODULE = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = MODULE
SPEC.loader.exec_module(MODULE)


#: The rustdoc invocation the fixtures pretend to run.
COMMAND = ("cargo", "rustdoc", "-p", "lash-core", "--lib")


def git(repo: Path, *arguments: str) -> None:
    subprocess.run(
        ["git", *arguments],
        cwd=repo,
        check=True,
        stdout=subprocess.DEVNULL,
        stderr=subprocess.DEVNULL,
    )


class CacheFixture:
    """A committed miniature workspace with the cargo/toolchain probes stubbed.

    The module keys on `git` and on `cargo metadata`; only the first is worth
    running for real, because its index-versus-worktree distinction is the
    whole subtlety being tested.  Metadata and the toolchain stamp are injected
    so the fixture needs no cargo project.
    """

    def __init__(self, root: Path) -> None:
        self.repo = (root / "repo").resolve()
        self.cache = root / "cache"
        self.target = root / "target"
        (self.repo / "crates" / "lash-core" / "src").mkdir(parents=True)
        (self.repo / "crates" / "dep" / "src").mkdir(parents=True)
        (self.repo / "crates" / "unrelated" / "src").mkdir(parents=True)
        self.write("Cargo.toml", "[workspace]\n")
        self.write("Cargo.lock", "version = 4\n")
        self.write("crates/lash-core/Cargo.toml", "[package]\nname = 'lash-core'\n")
        self.write("crates/lash-core/src/lib.rs", "pub fn one() {}\n")
        self.write("crates/dep/Cargo.toml", "[package]\nname = 'dep'\n")
        self.write("crates/dep/src/lib.rs", "pub fn two() {}\n")
        self.write("crates/unrelated/Cargo.toml", "[package]\nname = 'unrelated'\n")
        self.write("crates/unrelated/src/lib.rs", "pub fn three() {}\n")
        git(self.repo, "init", "-q", "-b", "main")
        git(self.repo, "config", "user.email", "fixture@example.invalid")
        git(self.repo, "config", "user.name", "Fixture")
        self.commit("initial")

        MODULE._METADATA[self.repo] = self.metadata()
        MODULE._TOOLCHAIN[self.repo] = "rustc 1.0.0 (fixture)\n"

    def metadata(self) -> dict:
        def package(name: str, identifier: str) -> dict:
            return {
                "id": identifier,
                "name": name,
                "version": "0.1.0",
                "manifest_path": str(self.repo / "crates" / name / "Cargo.toml"),
            }

        return {
            "packages": [
                package("lash-core", "core-id"),
                package("dep", "dep-id"),
                package("unrelated", "unrelated-id"),
                {
                    "id": "schemars-id",
                    "name": "schemars",
                    "version": "0.8.21",
                    "manifest_path": "/registry/schemars-0.8.21/Cargo.toml",
                },
            ],
            "workspace_members": ["core-id", "dep-id", "unrelated-id"],
            "resolve": {
                "nodes": [
                    {"id": "core-id", "deps": [{"pkg": "dep-id"}]},
                    {"id": "dep-id", "deps": []},
                    {"id": "unrelated-id", "deps": []},
                    {"id": "schemars-id", "deps": []},
                ]
            },
        }

    def write(self, relative: str, contents: str) -> None:
        path = self.repo / relative
        path.parent.mkdir(parents=True, exist_ok=True)
        path.write_text(contents, encoding="utf-8")

    def commit(self, message: str) -> None:
        git(self.repo, "add", "-A")
        git(self.repo, "commit", "-q", "-m", message)

    def key(self, package: str = "lash-core", command: tuple[str, ...] = COMMAND) -> str:
        return MODULE.cache_key(
            self.repo,
            package,
            "lash_core",
            command,
            MODULE.keyed_paths(self.repo, package),
        )

    def destination(self) -> Path:
        return self.target / "doc" / "lash_core.json"

    def ensure(self, body: str = "{}", package: str = "lash-core") -> tuple[Path, list[int]]:
        """Run `ensure`, writing `body` at the destination on every generation."""
        calls: list[int] = []

        def generate() -> None:
            calls.append(1)
            destination = self.destination()
            destination.parent.mkdir(parents=True, exist_ok=True)
            destination.write_text(body, encoding="utf-8")

        document = MODULE.ensure(
            repo=self.repo,
            package=package,
            crate_name="lash_core",
            command=COMMAND,
            destination=self.destination(),
            generate=generate,
        )
        return document, calls


class RustdocJsonCacheTest(unittest.TestCase):
    def setUp(self) -> None:
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        root = Path(self.directory.name)
        environment = unittest.mock.patch.dict(
            os.environ,
            {
                "LASH_RUSTDOC_CACHE_DIR": str(root / "cache"),
                "RUSTDOCFLAGS": "-D warnings",
            },
        )
        environment.start()
        self.addCleanup(environment.stop)
        os.environ.pop("LASH_RUSTDOC_CACHE", None)
        # The module narrates hits and misses on stderr; capture it so the
        # suite stays quiet and the narration itself stays assertable.
        reported = unittest.mock.patch("sys.stderr", new=io.StringIO())
        self.reported = reported.start()
        self.addCleanup(reported.stop)
        self.fixture = CacheFixture(root)
        self.addCleanup(MODULE._METADATA.clear)
        self.addCleanup(MODULE._TOOLCHAIN.clear)

    # -- keying ---------------------------------------------------------

    def test_key_is_stable_across_calls_and_timestamps(self) -> None:
        first = self.fixture.key()
        os.utime(self.fixture.repo / "crates" / "lash-core" / "src" / "lib.rs", (0, 0))
        self.assertEqual(first, self.fixture.key())

    def test_key_follows_the_package_and_its_path_dependencies(self) -> None:
        baseline = self.fixture.key()

        self.fixture.write("crates/lash-core/src/lib.rs", "pub fn one() { }\n")
        self.fixture.commit("touch the package")
        after_package = self.fixture.key()
        self.assertNotEqual(baseline, after_package)

        self.fixture.write("crates/dep/src/lib.rs", "pub fn two() { }\n")
        self.fixture.commit("touch the path dependency")
        after_dependency = self.fixture.key()
        self.assertNotEqual(after_package, after_dependency)

    def test_key_ignores_crates_the_package_does_not_reach(self) -> None:
        baseline = self.fixture.key()
        self.fixture.write("crates/unrelated/src/lib.rs", "pub fn three() { }\n")
        self.fixture.commit("touch an unreached crate")
        self.assertEqual(baseline, self.fixture.key())

    def test_key_follows_the_lockfile_command_flags_and_toolchain(self) -> None:
        baseline = self.fixture.key()

        self.assertNotEqual(baseline, self.fixture.key(command=(*COMMAND, "--all-features")))

        with unittest.mock.patch.dict(os.environ, {"RUSTDOCFLAGS": "-D warnings -Z x"}):
            self.assertNotEqual(baseline, self.fixture.key())

        MODULE._TOOLCHAIN[self.fixture.repo] = "rustc 1.1.0 (fixture)\n"
        self.assertNotEqual(baseline, self.fixture.key())
        MODULE._TOOLCHAIN[self.fixture.repo] = "rustc 1.0.0 (fixture)\n"

        self.fixture.write("Cargo.lock", "version = 4\n# moved\n")
        self.fixture.commit("move the lockfile")
        self.assertNotEqual(baseline, self.fixture.key())

    def test_registry_package_is_keyed_on_workspace_manifests(self) -> None:
        """A third-party document contributes no crate directory of its own."""
        paths = MODULE.keyed_paths(self.fixture.repo, "schemars@0.8")
        self.assertIn("Cargo.toml", paths)
        self.assertIn("crates/lash-core/Cargo.toml", paths)
        self.assertNotIn("crates/lash-core", paths)

        baseline = self.fixture.key(package="schemars@0.8")
        self.fixture.write("crates/lash-core/Cargo.toml", "[package]\nname = 'lash-core'\n# f\n")
        self.fixture.commit("change a feature declaration")
        self.assertNotEqual(baseline, self.fixture.key(package="schemars@0.8"))

    def test_version_qualified_spec_selects_its_major(self) -> None:
        with self.assertRaises(KeyError):
            MODULE.keyed_paths(self.fixture.repo, "schemars@0.9")

    # -- hit, miss, bypass ----------------------------------------------

    def test_miss_generates_and_populates_then_hit_reuses(self) -> None:
        document, calls = self.fixture.ensure(body='{"root": 1}')
        self.assertEqual(calls, [1])
        self.assertEqual(document, self.fixture.destination())
        entry = Path(os.environ["LASH_RUSTDOC_CACHE_DIR"]) / f"{self.fixture.key()}.json"
        self.assertTrue(entry.exists())

        document, calls = self.fixture.ensure(body='{"root": 2}')
        self.assertEqual(calls, [])
        self.assertEqual(document, entry)
        self.assertEqual(json.loads(document.read_text(encoding="utf-8")), {"root": 1})

    def test_hit_leaves_cargos_own_output_untouched(self) -> None:
        """Cargo's freshness is fingerprint-based, so its output must stay its own."""
        self.fixture.ensure(body='{"root": 1}')
        self.fixture.destination().write_text('{"other-configuration": true}', encoding="utf-8")
        document, calls = self.fixture.ensure()
        self.assertEqual(calls, [])
        self.assertNotEqual(document, self.fixture.destination())
        self.assertEqual(
            json.loads(self.fixture.destination().read_text(encoding="utf-8")),
            {"other-configuration": True},
        )

    def test_dirty_tree_bypasses_the_cache_entirely(self) -> None:
        self.fixture.write("crates/lash-core/src/lib.rs", "pub fn one() { /* edited */ }\n")
        document, calls = self.fixture.ensure(body='{"dirty": true}')
        self.assertEqual(calls, [1])
        self.assertEqual(document, self.fixture.destination())
        self.assertIn("bypass -- 1 uncommitted path(s)", self.reported.getvalue())
        self.assertEqual(list(Path(os.environ["LASH_RUSTDOC_CACHE_DIR"]).glob("*.json")), [])

        # And a bypassed run does not read a previously stored entry either.
        self.fixture.commit("record the edit")
        self.fixture.ensure(body='{"clean": true}')
        self.fixture.write("crates/dep/src/lib.rs", "pub fn two() { /* edited */ }\n")
        _, calls = self.fixture.ensure(body='{"dirty": true}')
        self.assertEqual(calls, [1])

    def test_untracked_file_under_a_keyed_crate_bypasses(self) -> None:
        self.fixture.write("crates/dep/src/extra.rs", "pub fn four() {}\n")
        _, calls = self.fixture.ensure()
        self.assertEqual(calls, [1])
        self.assertEqual(list(Path(os.environ["LASH_RUSTDOC_CACHE_DIR"]).glob("*.json")), [])

    def test_edit_outside_the_keyed_crates_still_caches(self) -> None:
        self.fixture.write("crates/unrelated/src/lib.rs", "pub fn three() { /* edited */ }\n")
        _, calls = self.fixture.ensure()
        self.assertEqual(calls, [1])
        _, calls = self.fixture.ensure()
        self.assertEqual(calls, [])

    def test_switching_the_cache_off_never_reads_or_writes(self) -> None:
        with unittest.mock.patch.dict(os.environ, {"LASH_RUSTDOC_CACHE": "off"}):
            self.assertIsNone(MODULE.cache_dir())
            _, calls = self.fixture.ensure()
            self.assertEqual(calls, [1])
            _, calls = self.fixture.ensure()
            self.assertEqual(calls, [1])
        self.assertEqual(list(Path(os.environ["LASH_RUSTDOC_CACHE_DIR"]).glob("*.json")), [])

    def test_default_location_is_the_user_cache_root(self) -> None:
        root = Path(self.directory.name) / "xdg"
        with unittest.mock.patch.dict(
            os.environ, {"XDG_CACHE_HOME": str(root), "LASH_RUSTDOC_CACHE_DIR": ""}
        ):
            self.assertEqual(MODULE.cache_dir(), root / "lash" / "rustdoc-json")
        self.assertTrue((root / "lash" / "rustdoc-json").is_dir())

    def test_an_uncreatable_cache_directory_disables_rather_than_fails(self) -> None:
        blocker = Path(self.directory.name) / "file"
        blocker.write_text("not a directory", encoding="utf-8")
        with unittest.mock.patch.dict(
            os.environ, {"LASH_RUSTDOC_CACHE_DIR": str(blocker / "cache")}
        ):
            self.assertIsNone(MODULE.cache_dir())
            _, calls = self.fixture.ensure()
        self.assertEqual(calls, [1])

    def test_unknown_package_generates_rather_than_failing(self) -> None:
        _, calls = self.fixture.ensure(package="not-a-package")
        self.assertEqual(calls, [1])

    # -- storage ---------------------------------------------------------

    def test_populate_is_atomic_and_leaves_no_partial_files(self) -> None:
        cache = Path(os.environ["LASH_RUSTDOC_CACHE_DIR"])
        replaced: list[tuple[Path, Path]] = []
        real_replace = os.replace

        def record(source, destination, *arguments, **keywords):
            replaced.append((Path(source), Path(destination)))
            # The visible name must not exist until the rename publishes it.
            self.assertFalse(Path(destination).exists())
            return real_replace(source, destination, *arguments, **keywords)

        with unittest.mock.patch.object(MODULE.os, "replace", side_effect=record):
            self.fixture.ensure(body='{"root": 1}')

        self.assertEqual(len(replaced), 1)
        source, destination = replaced[0]
        self.assertEqual(source.parent, cache)
        self.assertEqual(destination, cache / f"{self.fixture.key()}.json")
        self.assertEqual([path.name for path in cache.iterdir()], [destination.name])

    def test_failed_generation_stores_nothing(self) -> None:
        def generate() -> None:
            raise subprocess.CalledProcessError(1, COMMAND)

        with self.assertRaises(subprocess.CalledProcessError):
            MODULE.ensure(
                repo=self.fixture.repo,
                package="lash-core",
                crate_name="lash_core",
                command=COMMAND,
                destination=self.fixture.destination(),
                generate=generate,
            )
        self.assertEqual(list(Path(os.environ["LASH_RUSTDOC_CACHE_DIR"]).glob("*.json")), [])

    def test_prune_drops_the_least_recently_used_entries_first(self) -> None:
        cache = Path(os.environ["LASH_RUSTDOC_CACHE_DIR"])
        cache.mkdir(parents=True, exist_ok=True)
        for index in range(4):
            entry = cache / f"{index:064x}.json"
            entry.write_bytes(b"x" * 100)
            os.utime(entry, (index, index))
        removed = MODULE.prune(cache, budget=250)
        self.assertEqual(removed, 2)
        self.assertEqual(
            sorted(path.name for path in cache.glob("*.json")),
            [f"{2:064x}.json", f"{3:064x}.json"],
        )

    def test_a_hit_refreshes_the_entry_against_pruning(self) -> None:
        self.fixture.ensure()
        entry = Path(os.environ["LASH_RUSTDOC_CACHE_DIR"]) / f"{self.fixture.key()}.json"
        os.utime(entry, (0, 0))
        self.fixture.ensure()
        self.assertGreater(entry.stat().st_mtime, 0)

    # -- the shell entry point --------------------------------------------

    def test_command_line_runs_the_command_then_reports_the_hit_path(self) -> None:
        destination = self.fixture.destination()
        destination.parent.mkdir(parents=True, exist_ok=True)
        argv = [
            "--repo",
            str(self.fixture.repo),
            "--package",
            "lash-core",
            "--crate-name",
            "lash_core",
            "--destination",
            str(destination),
            "--",
            sys.executable,
            "-c",
            f"open({str(destination)!r}, 'w').write('{{\"root\": 7}}')",
        ]
        with unittest.mock.patch("sys.stdout", new=io.StringIO()) as first:
            self.assertEqual(MODULE.main(argv), 0)
        self.assertEqual(first.getvalue().strip(), str(destination))

        destination.unlink()
        with unittest.mock.patch("sys.stdout", new=io.StringIO()) as second:
            self.assertEqual(MODULE.main(argv), 0)
        reported = Path(second.getvalue().strip())
        self.assertFalse(destination.exists(), "a hit must not re-run the command")
        self.assertEqual(json.loads(reported.read_text(encoding="utf-8")), {"root": 7})


if __name__ == "__main__":
    unittest.main()
