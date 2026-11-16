"""A land runs the scripts/land of the tree it lands (FIG-7694, job F1)."""
import contextlib
import importlib.machinery
import importlib.util
import io
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import unittest
from unittest.mock import patch

SCRIPTS = Path(__file__).resolve().parent
# scripts/land and every module it loads from scripts/ when it starts.
CODE = ("land", "apply_format_witnesses.py", "check_version_bumps.py", "discover_version_surfaces.py",
        "perf_land_gate.py", "perf_land_ceilings.py")


def git(cwd, *args):
    return subprocess.run(["git", "-c", "user.name=t", "-c", "user.email=t@t", *args], cwd=cwd,
                          check=True, capture_output=True, text=True).stdout.strip()


class Repositories:
    """A bare origin whose main carries this tree's land code, and a
    candidate clone one commit ahead of it."""

    def __init__(self, root):
        self.origin, self.candidate, self.other = root / "origin.git", root / "candidate", root / "other"
        git(root, "init", "-q", "--bare", "-b", "main", str(self.origin))
        git(root, "clone", "-q", str(self.origin), str(self.other))
        (self.other / "scripts").mkdir()
        for name in CODE:
            shutil.copy2(SCRIPTS / name, self.other / "scripts" / name)
        (self.other / "README").write_text("main\n")
        self.publish("main")
        git(root, "clone", "-q", str(self.origin), str(self.candidate))
        (self.candidate / "work.txt").write_text("candidate\n")
        git(self.candidate, "add", "-A")
        git(self.candidate, "commit", "-qm", "candidate")

    def publish(self, message, **files):
        for name, text in files.items():
            (self.other / name).write_text(text)
        git(self.other, "add", "-A")
        git(self.other, "commit", "-qm", message)
        git(self.other, "push", "-q", "origin", "HEAD:main")

    def land(self, runs=None):
        env = {k: v for k, v in os.environ.items() if k != "LASH_LAND_RUNS"}
        if runs:
            env["LASH_LAND_RUNS"] = str(runs)
        env.update(GIT_AUTHOR_NAME="t", GIT_AUTHOR_EMAIL="t@t", GIT_COMMITTER_NAME="t",
                   GIT_COMMITTER_EMAIL="t@t", PYTHONDONTWRITEBYTECODE="1")
        result = subprocess.run([sys.executable, "scripts/land", "--dry-run"], cwd=self.candidate, env=env,
                                stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True, timeout=300)
        return result.stdout


NEW_LAND = "import sys\nprint('the rebased scripts/land ran', sys.argv[1:])\n"


class LandRunsItsOwnTree(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.repos = Repositories(Path(directory.name))

    def test_a_rebase_that_changes_scripts_land_continues_under_the_new_one(self):
        self.repos.publish("new land", **{"scripts/land": NEW_LAND})
        output = self.repos.land()
        self.assertIn("land: restarting from the rebased tree (changed land)", output)
        self.assertIn("the rebased scripts/land ran ['--dry-run']", output)

    def test_a_rebase_that_changes_a_module_land_loaded_continues_under_the_new_one(self):
        self.repos.publish("new gate", **{"scripts/perf_land_gate.py": "raise SystemExit('the rebased gate module ran')\n"})
        output = self.repos.land()
        self.assertIn("(changed perf_land_gate.py)", output)
        self.assertIn("the rebased gate module ran", output)

    def test_a_rebase_that_changes_no_land_code_does_not_restart(self):
        self.repos.publish("other work", README="moved\n")
        output = self.repos.land()
        self.assertIn("Successfully rebased", output)
        self.assertNotIn("restarting", output)

    def test_restarts_stop_at_the_run_budget(self):
        self.repos.publish("new land", **{"scripts/land": NEW_LAND})
        output = self.repos.land(runs=12)
        self.assertIn("land: main kept moving: 12 runs of scripts/land", output)
        self.assertNotIn("the rebased scripts/land ran", output)


class Restart(Exception):
    pass


class ALostPushRestarts(unittest.TestCase):
    """A push lost to a moving main restarts the land, so the next rebase is
    again the first of a process and is checked for changed land code."""

    def test_a_push_lost_to_a_moving_main_restarts_instead_of_rebasing_in_process(self):
        with tempfile.TemporaryDirectory() as directory:
            repos = Repositories(Path(directory))
            loader = importlib.machinery.SourceFileLoader("land_under_test", str(SCRIPTS / "land"))
            spec = importlib.util.spec_from_loader(loader.name, loader)
            land = importlib.util.module_from_spec(spec)
            loader.exec_module(land)
            real_run = land.run

            def run(*args, check=True, quiet=False):
                if args[0] == "git":
                    return real_run(*args, check=check, quiet=quiet)
                if "--out" in args:  # the perf gate: a qualified receipt with no ratchet
                    out = Path(args[args.index("--out") + 1])
                    out.mkdir(parents=True, exist_ok=True)
                    (out / "receipt.json").write_text(json.dumps({"ratchet": {}}))
                return subprocess.CompletedProcess(args, 0, "", "")

            def execv(*args):
                raise Restart(args)

            cwd = os.getcwd()
            os.chdir(repos.candidate)
            pushes = []
            try:
                with patch.object(land, "run", run), \
                        patch.object(land, "apply_format_witnesses", lambda *a: []), \
                        patch.object(land, "land_lock", contextlib.nullcontext), \
                        patch.object(land, "push", lambda *a: pushes.append(a) and False), \
                        patch.object(land.os, "execv", execv), \
                        patch.object(sys, "argv", ["scripts/land"]), \
                        patch.dict(os.environ, {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
                                                "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t"}), \
                        contextlib.redirect_stdout(io.StringIO()):
                    os.environ.pop("LASH_LAND_RUNS", None)
                    with self.assertRaises(Restart):
                        land.main()
            finally:
                os.chdir(cwd)
            self.assertEqual(len(pushes), 1)


if __name__ == "__main__":
    unittest.main()
