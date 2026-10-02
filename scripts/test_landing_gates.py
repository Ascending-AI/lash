#!/usr/bin/env python3
"""Landing verdicts against reset sources and the real in-process replay law.

The scratch repository owns the version comparison and corpus mutations. Its
Kiln launcher forwards replay to this isolated fork, where JOURNAL_LOGIC_EPOCH
is already the reset epoch. No service, reset-tree build or CI dispatch runs.
"""

from pathlib import Path
import json
import os
import shlex
import shutil
import subprocess
import tarfile
import tempfile
import unittest

import release_baseline as baseline
import release_reset as reset

ROOT = Path(__file__).resolve().parents[1]
GATE = "scripts/ci/landing-gates.sh"
VERSION_GATE = "scripts/ci/version-bump-gate.sh"
LAW = "tests::replay_corpus::replay_corpus_fixtures_match_current_controller"
CORPUS = "fixtures/release/v1.0.0/replay-corpus"
PROOF = ROOT / ".buck2/landing-proof"


class LandingGatesTests(unittest.TestCase):
    def setUp(self):
        PROOF.mkdir(parents=True, exist_ok=True)
        temporary = tempfile.TemporaryDirectory(dir=PROOF)
        self.addCleanup(temporary.cleanup)
        self.repo = Path(temporary.name)
        self.env = dict(os.environ)
        self.env.pop("LASH_REPLAY_CORPUS_ROOT", None)

    def git(self, *args):
        return subprocess.run(
            ["git", "-c", "user.name=fixture", "-c", "user.email=fixture@example.invalid",
             "-c", "commit.gpgsign=false", "-c", "gc.auto=0", *args],
            cwd=self.repo, check=True, capture_output=True, text=True,
        ).stdout.strip()

    def commit(self, message):
        self.git("add", "--all")
        self.git("commit", "--quiet", "--allow-empty", "--message", message)
        return self.git("rev-parse", "HEAD")

    def invoke(self, *args):
        return subprocess.run(
            ["bash", str(self.repo / GATE), *args], cwd=self.repo,
            env=self.env, capture_output=True, text=True,
        )

    def copy_gates(self):
        for relative in (GATE, VERSION_GATE):
            source = ROOT / relative
            if source.exists():
                destination = self.repo / relative
                destination.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, destination)

    def post_reset_repo(self):
        archive = PROOF / "source.tar"
        with archive.open("wb") as output:
            subprocess.run(["git", "archive", "HEAD"], cwd=ROOT, stdout=output, check=True)
        with tarfile.open(archive) as source:
            source.extractall(self.repo, filter="data")
        self.copy_gates()
        _, edits = reset.plan(self.repo)
        for path, text in edits.items():
            path.write_text(text)
        self.assertEqual(baseline.mismatches(baseline.inventory(self.repo)), [])
        shutil.copytree(ROOT / "crates/lash-restate/testdata/replay-corpus", self.repo / CORPUS)
        for fixture in (self.repo / CORPUS).glob("*/journal.json"):
            self.assertEqual(json.loads(fixture.read_text())["journal_logic_epoch"], 1)

        # Only the executable's location differs from a lander's invocation.
        # Its real report, arguments and mutated corpus are preserved.
        kiln = shutil.which("kiln")
        self.assertIsNotNone(kiln)
        launcher_dir = self.repo / ".test-bin"
        launcher_dir.mkdir()
        launcher = launcher_dir / "kiln"
        launcher.write_text(
            "#!/usr/bin/env bash\nset -euo pipefail\n"
            f"cd {shlex.quote(str(ROOT))}\n"
            f"exec {shlex.quote(kiln)} \"$@\"\n"
        )
        launcher.chmod(0o755)
        self.env["PATH"] = str(launcher_dir) + os.pathsep + self.env["PATH"]
        self.git("init", "--quiet", "--initial-branch=main")
        return self.commit("post-reset baseline")

    def test_post_reset_landing_verdicts(self):
        base = self.post_reset_repo()

        unchanged = self.invoke(base, base)
        (PROOF / "unchanged.log").write_text(unchanged.stdout + unchanged.stderr)
        self.assertEqual(unchanged.returncode, 0, unchanged.stdout + unchanged.stderr)
        self.assertIn("1 replay law passed", unchanged.stdout)
        self.keep_report("unchanged")

        journal = self.repo / CORPUS / "scalar-lashlang-tool-attempt/journal.json"
        fixture = json.loads(journal.read_text())
        fixture["journal_steps"].append("lash:release-journal-added-step")
        fixture["records"]["lash:release-journal-added-step"] = next(iter(fixture["records"].values()))
        journal.write_text(json.dumps(fixture, indent=2) + "\n")
        head = self.commit("added recorded step with unchanged epoch")
        replay = self.invoke(base, head)
        (PROOF / "added-step.log").write_text(replay.stdout + replay.stderr)
        self.assertNotEqual(replay.returncode, 0, replay.stdout + replay.stderr)
        self.assertIn("journal logic changed: bump JOURNAL_LOGIC_EPOCH", replay.stdout + replay.stderr)
        self.keep_report("added-step")
        print("post-reset replay verdicts: unchanged=0, added-step=nonzero")

    def test_post_reset_unbumped_shape_fails(self):
        base = self.post_reset_repo()
        shape = self.repo / "crates/lash-remote-protocol/src/turn_result.rs"
        original = shape.read_text()
        self.assertIn("pub struct RemoteTurnReport {", original)
        shape.write_text(original.replace("pub struct RemoteTurnReport {",
                                          "pub struct RemoteTurnReport {\n    pub planted: String,", 1))
        head = self.commit("unbumped guarded shape")
        unbumped = self.invoke(base, head)
        (PROOF / "unbumped.log").write_text(unbumped.stdout + unbumped.stderr)
        self.assertEqual(unbumped.returncode, 1, unbumped.stdout + unbumped.stderr)
        self.assertIn("version-bump check failed", unbumped.stderr)
        self.assertIn("REMOTE_PROTOCOL_VERSION is 1 on both sides", unbumped.stderr)
        print("post-reset unbumped guarded edit: exit 1")

    def keep_report(self, name):
        reports = list((self.repo / ".buck2/landing-gates").glob("*/test-report.json"))
        self.assertTrue(reports, "Kiln must write a replay execution report")
        report = max(reports, key=lambda path: path.stat().st_mtime_ns)
        data = json.loads(report.read_text())
        self.assertTrue(data["session_complete"])
        results = list(data["results"].values())
        self.assertEqual(len(results), 1)
        result = results[0]
        self.assertIn(f"test {LAW} ...", result["stdout"])
        self.assertIn("1 passed" if name == "unchanged" else "1 failed", result["stdout"])
        shutil.copy2(report, PROOF / f"{name}.json")
        shutil.copy2(result["outputs"]["junit_xml"], PROOF / f"{name}.xml")
        print(f"{name}: 1 replay case executed; evidence {PROOF / (name + '.json')}")

    def minimal_repo(self):
        self.copy_gates()
        self.git("init", "--quiet", "--initial-branch=main")
        (self.repo / CORPUS).mkdir(parents=True)
        (self.repo / "tracked").write_text("baseline\n")
        return self.commit("baseline")

    def test_an_unresolvable_baseline_fails(self):
        head = self.minimal_repo()
        result = self.invoke("0" * 40, head)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("fatal", result.stderr)

    def test_a_different_candidate_checkout_fails(self):
        base = self.minimal_repo()
        head = self.commit("candidate")
        result = self.invoke(base, base)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("candidate must be checked out", result.stderr)
        self.assertNotEqual(base, head)

    def test_a_dirty_candidate_checkout_fails(self):
        head = self.minimal_repo()
        (self.repo / "tracked").write_text("dirty\n")
        result = self.invoke(head, head)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("tracked edits", result.stderr)

    def test_a_missing_corpus_fails(self):
        head = self.minimal_repo()
        shutil.rmtree(self.repo / CORPUS)
        result = self.invoke(head, head)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("replay corpus", result.stderr)


if __name__ == "__main__":
    unittest.main()
