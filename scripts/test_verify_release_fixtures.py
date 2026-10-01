#!/usr/bin/env python3
"""Release corpus integrity and read-back laws, opt-in until FIG-4495."""

import contextlib
import hashlib
import importlib
import io
import json
import os
from pathlib import Path
import sqlite3
import subprocess
import tempfile
import unittest
from unittest import mock

import capture_release_fixtures as capture


class ReleaseFixtureLaws(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name)
        # Repo-attachment variables leak out of hooks, wrappers and CI
        # environments; neither the fixture plumbing nor the verifier may
        # honor them, so the whole fixture runs under a polluted set.
        ambient = mock.patch.dict(os.environ, {
            "GIT_DIR": str(self.repo / "ambient.git"),
            "GIT_WORK_TREE": str(self.repo / "ambient-tree"),
            "GIT_INDEX_FILE": str(self.repo / "ambient-index"),
            "GIT_NAMESPACE": "ambient",
            "GIT_CONFIG_PARAMETERS": "'core.bare=true'",
        })
        ambient.start()
        self.addCleanup(ambient.stop)
        # The synthetic repo needs no ambient git configuration either: point
        # both files at a path that does not exist.
        self.git_environment = {
            **capture.git_env(),
            "GIT_CONFIG_GLOBAL": str(self.repo / "gitconfig-none"),
            "GIT_CONFIG_SYSTEM": str(self.repo / "gitconfig-none"),
        }
        for args in (
            ["init", "-q"],
            ["-c", "user.name=Fixture", "-c", "user.email=fixture@example.invalid",
             "commit", "--allow-empty", "-qm", "fixture baseline"],
            ["tag", "v1.0.0"],
        ):
            subprocess.run(["git", *args], cwd=self.repo, check=True,
                           capture_output=True, env=self.git_environment)
        self.dest = self.repo / "corpus"
        self.dest.mkdir()
        self.manifest_legs = []
        for leg in capture.LEGS:
            root = self.dest / leg.name
            root.mkdir()
            path = root / "artifact.json"
            path.write_text('{"fixture": "retained"}\n')
            if leg.name in ("sqlite-stores", "session-at-rest"):
                path = root / "durable-core.db"
                with sqlite3.connect(path) as db:
                    db.execute("CREATE TABLE fixture (value TEXT)")
                    db.execute("INSERT INTO fixture VALUES ('retained')")
            self.manifest_legs.append({
                "name": leg.name,
                "files": [self.record(file, root) for file in sorted(root.iterdir())],
            })
        commit = capture.git(self.repo, "rev-parse", "HEAD")
        (self.dest / "manifest.json").write_text(json.dumps({
            "schema": capture.MANIFEST_SCHEMA, "tag": "v1.0.0",
            "source_commit": commit, "dry_run": False,
            "legs": self.manifest_legs,
        }))
        self.original = (self.dest / "manifest.json").read_bytes()

    @staticmethod
    def record(path, root):
        data = path.read_bytes()
        return {"path": path.relative_to(root).as_posix(),
                "sha256": hashlib.sha256(data).hexdigest(), "bytes": len(data)}

    def write_manifest(self, mutate):
        manifest = json.loads(self.original)
        mutate(manifest)
        (self.dest / "manifest.json").write_text(json.dumps(manifest))

    def test_release_fixture_manifest_law(self):
        verify = importlib.import_module("verify_release_fixtures")
        self.assertGreater(len(verify.verify(self.dest, self.repo)), 0)
        mutations = (
            ("missing leg", lambda m: m["legs"].pop()),
            ("source_commit", lambda m: m.update(source_commit="0" * 40)),
            ("dry_run", lambda m: m.update(dry_run=True)),
            ("schema", lambda m: m.update(schema="foreign")),
            ("tag", lambda m: m.update(tag="missing-tag")),
            ("bytes", lambda m: m["legs"][0]["files"][0].update(bytes=1)),
            ("unsafe path", lambda m: m["legs"][0]["files"][0].update(path="../manifest.json")),
            ("duplicate", lambda m: m["legs"].append(m["legs"][0])),
        )
        for name, mutate in mutations:
            with self.subTest(name=name):
                self.write_manifest(mutate)
                with self.assertRaises(verify.VerificationError):
                    verify.verify(self.dest, self.repo)
        (self.dest / "manifest.json").write_bytes(self.original)
        path = self.dest / "replay-corpus/artifact.json"
        original = path.read_bytes()
        path.write_bytes(original.replace(b"retained", b"mutated!"))
        with self.assertRaisesRegex(verify.VerificationError, "sha256"):
            verify.verify(self.dest, self.repo)
        path.write_bytes(original)
        with self.assertRaisesRegex(verify.VerificationError, "service identity"):
            verify.verify(self.dest, self.repo, forbidden=[b"retained"])
        path.unlink()
        with self.assertRaises(verify.VerificationError):
            verify.verify(self.dest, self.repo)

    def test_release_fixture_read_back_law(self):
        read_back = importlib.import_module("read_release_fixtures")
        with contextlib.redirect_stdout(io.StringIO()) as output:
            self.assertGreater(read_back.read_corpus(self.dest, self.repo), 0)
        self.assertRegex(output.getvalue(), r"read back [1-9][0-9]* fixtures")
        path = self.dest / "replay-corpus/artifact.json"
        original = path.read_bytes()
        path.write_bytes(b"{" + original[1:-2])
        # Refresh integrity metadata: the read-back must catch malformed
        # serialized bytes independently of the hash oracle.
        self.write_manifest(lambda m: m["legs"][-1].update(files=[self.record(path, path.parent)]))
        with self.assertRaises(read_back.ReadBackError):
            read_back.read_corpus(self.dest, self.repo)
        path.write_bytes(original)
        (self.dest / "manifest.json").write_bytes(self.original)
        path.write_bytes(original.replace(b"retained", b"mutated!"))
        with self.assertRaises(read_back.ReadBackError):
            read_back.read_corpus(self.dest, self.repo)
        path.unlink()
        with self.assertRaises(read_back.ReadBackError):
            read_back.read_corpus(self.dest, self.repo)


if __name__ == "__main__":
    unittest.main()
