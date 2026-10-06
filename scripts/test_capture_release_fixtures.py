#!/usr/bin/env python3
"""Fixture tests for capture_release_fixtures.py."""

from __future__ import annotations

import hashlib
import importlib.util
import json
from pathlib import Path
import sys
import subprocess
from unittest.mock import patch
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("capture_release_fixtures.py")
SPEC = importlib.util.spec_from_file_location("capture_release_fixtures", SCRIPT)
assert SPEC is not None and SPEC.loader is not None
capture = importlib.util.module_from_spec(SPEC)
sys.modules[SPEC.name] = capture
SPEC.loader.exec_module(capture)


def plant_tree(root: Path) -> None:
    for leg in capture.LEGS:
        if leg.name == "replay-corpus":
            source = root / leg.source / "scenario"
            source.mkdir(parents=True, exist_ok=True)
            (source / "journal.json").write_text(json.dumps({"generation": "0123456789ab"}) + "\n")
            continue
        source = root / leg.source
        if leg.source.endswith(".db"):
            source.parent.mkdir(parents=True, exist_ok=True)
            source.write_bytes(b"fake sqlite bytes " + leg.name.encode())
        else:
            source.mkdir(parents=True, exist_ok=True)
            (source / "artifact.json").write_text(
                json.dumps({"leg": leg.name}) + "\n"
            )
            (source / "nested").mkdir()
            (source / "nested" / "data.bin").write_bytes(b"\x00\x01")


class CaptureTests(unittest.TestCase):
    def setUp(self):
        self.temp = tempfile.TemporaryDirectory()
        self.addCleanup(self.temp.cleanup)
        self.repo = Path(self.temp.name) / "repo"
        self.repo.mkdir()
        plant_tree(self.repo)
        self.dest = Path(self.temp.name) / "out"

    def run_capture(self, *args: str) -> int:
        return capture.main(
            ["--repo", str(self.repo), "--tag", "v1.0", "--dest", str(self.dest), *args]
        )

    def test_dry_run_copies_every_leg_and_writes_a_manifest(self):
        self.assertEqual(self.run_capture("--dry-run"), 0)
        manifest = json.loads((self.dest / "manifest.json").read_text())
        self.assertEqual(manifest["schema"], capture.MANIFEST_SCHEMA)
        self.assertEqual(manifest["tag"], "v1.0")
        self.assertIsNone(manifest["source_commit"])
        self.assertTrue(manifest["dry_run"])
        self.assertEqual(
            [leg["name"] for leg in manifest["legs"]],
            [leg.name for leg in capture.LEGS],
        )
        for leg in manifest["legs"]:
            self.assertGreaterEqual(len(leg["files"]), 1)
            for record in leg["files"]:
                copied = self.dest / leg["name"] / record["path"]
                self.assertEqual(
                    hashlib.sha256(copied.read_bytes()).hexdigest(), record["sha256"]
                )
        self.assertEqual(
            (self.dest / "session-at-rest" / "lash.db").read_bytes(),
            (self.repo / "fixtures/durable-read/v1/sqlite/lash.db").read_bytes(),
        )

    def test_the_source_commit_is_the_manifests_only_provenance(self):
        self.assertEqual(self.run_capture("--dry-run"), 0)
        manifest = json.loads((self.dest / "manifest.json").read_text())
        self.assertEqual(
            set(manifest),
            {"schema", "tag", "source_commit", "dry_run", "captured_at", "legs"},
        )

    def test_journals_of_two_generations_are_refused(self):
        other = self.repo / "crates/lash-restate/testdata/replay-corpus/other"
        other.mkdir()
        (other / "journal.json").write_text(json.dumps({"generation": "ba9876543210"}) + "\n")
        self.assertEqual(self.run_capture("--dry-run"), 2)

    def test_a_journal_without_a_generation_is_refused(self):
        journal = self.repo / "crates/lash-restate/testdata/replay-corpus/scenario/journal.json"
        journal.write_text(json.dumps({"journal_logic_epoch": 7}) + "\n")
        self.assertEqual(self.run_capture("--dry-run"), 2)

    def test_dry_run_without_dest_lands_in_a_temp_dir(self):
        self.assertEqual(capture.main(
            ["--repo", str(self.repo), "--tag", "v1.0", "--dry-run"]
        ), 0)

    def test_missing_source_fails_closed(self):
        shutil_target = self.repo / "crates/lash-restate/testdata/replay-corpus"
        for path in sorted(shutil_target.rglob("*"), reverse=True):
            path.rmdir() if path.is_dir() else path.unlink()
        shutil_target.rmdir()
        self.assertEqual(self.run_capture("--dry-run"), 2)

    def test_existing_nonempty_dest_is_refused(self):
        self.dest.mkdir()
        (self.dest / "stale").write_text("previous capture")
        self.assertEqual(self.run_capture("--dry-run"), 2)

    def test_unsafe_tag_is_refused(self):
        with self.assertRaises(SystemExit):
            capture.parse_args(["--tag", "../escape", "--dry-run"])

    def test_dry_run_and_regenerate_conflict(self):
        with self.assertRaises(SystemExit):
            capture.parse_args(["--tag", "v1.0", "--dry-run", "--regenerate"])

    def test_real_run_without_the_tagged_checkout_fails(self):
        # The synthetic tree is not a git worktree, so tag verification must
        # report a capture error rather than capture the wrong commit.
        self.assertEqual(self.run_capture(), 2)

    def test_a_regeneration_must_leave_tagged_fixtures_unchanged(self):
        def git(*args):
            return subprocess.run(["git", "-C", str(self.repo), "-c", "user.name=capture",
                                   "-c", "user.email=capture@example.invalid", *args],
                                  check=True, capture_output=True)
        git("init", "--quiet")
        git("add", "--all")
        git("commit", "--quiet", "-m", "tagged fixtures")
        capture.require_committed_fixtures_unchanged(self.repo, capture.LEGS)
        fixture = self.repo / "crates/lash-restate/testdata/replay-corpus/scenario/journal.json"
        fixture.write_text("different build\n")
        with self.assertRaises(capture.CaptureError):
            capture.require_committed_fixtures_unchanged(self.repo, capture.LEGS)
        git("add", str(fixture.relative_to(self.repo)))
        with self.assertRaises(capture.CaptureError):
            capture.require_committed_fixtures_unchanged(self.repo, capture.LEGS)

    def test_generator_switches_reach_the_kiln_test_action(self):
        leg = next(leg for leg in capture.LEGS if leg.name == "replay-corpus")
        with patch.object(capture.subprocess, "run", return_value=subprocess.CompletedProcess([], 0)) as run:
            capture.run_generators(self.repo, (leg,))
        command = run.call_args.args[0]
        self.assertIn("--test_env=LASH_REGENERATE=1", command)
        self.assertEqual(run.call_args.kwargs["env"]["BUILD_WORKSPACE_DIRECTORY"], str(self.repo))

    def test_regeneration_uses_owning_kiln_targets(self):
        for leg in capture.LEGS:
            for environment, _argv in leg.regenerate:
                self.assertEqual(environment, {"LASH_REGENERATE": "1"})
        commands = [
            argv
            for leg in capture.LEGS
            for _environment, argv in leg.regenerate
        ]
        self.assertGreater(len(commands), 0)
        for argv in commands:
            self.assertEqual(argv[:2], ["kiln", "test"])
            self.assertIn("--test_arg=--exact", argv)
            self.assertIn("--test_arg=--ignored", argv)
            self.assertTrue(argv[2].startswith("//crates/"))


if __name__ == "__main__":
    unittest.main()
