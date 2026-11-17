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

sys.path.insert(0, str(Path(__file__).resolve().parent))
from perf_land_gate import policy_values  # noqa: E402

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

    def land(self, runs=None, store=True):
        env = {k: v for k, v in os.environ.items() if k not in ("LASH_LAND_RUNS", "LASH_BUCK2_STORE")}
        env["HOME"] = str(self.other.parent)
        if store:
            env["LASH_BUCK2_STORE"] = str(self.other.parent / "buck2-store")
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

    def test_a_land_without_the_fork_environment_refuses_before_touching_the_tree(self):
        self.repos.publish("other work", README="moved\n")
        before = git(self.repos.candidate, "rev-parse", "HEAD")
        output = self.repos.land(store=False)
        self.assertIn("LASH_BUCK2_STORE is unset", output)
        self.assertIn(". ./env.sh", output)
        self.assertEqual(git(self.repos.candidate, "rev-parse", "HEAD"), before)
        self.assertFalse((self.repos.other.parent / ".cache").exists())


class Restart(Exception):
    pass


def land_in_process(repos, ratchet=None, inside_lock=None):
    """Run this tree's scripts/land main() against the fixture repositories,
    with qualification stubbed: every git operation, commit and hook is
    real. The perf gate's receipt carries `ratchet`; `inside_lock` runs on
    entering the land lock. Answers the land, the qualified head, the heads
    handed to push, and what main() raised."""
    loader = importlib.machinery.SourceFileLoader("land_under_test", str(SCRIPTS / "land"))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    land = importlib.util.module_from_spec(spec)
    loader.exec_module(land)
    real_run = land.run
    qualified, pushes = [], []

    def run(*args, check=True, quiet=False):
        if args[0] == "git":
            return real_run(*args, check=check, quiet=quiet)
        if "--out" in args:  # the perf gate: a qualified receipt
            qualified.append(land.git("rev-parse", "HEAD"))
            out = Path(args[args.index("--out") + 1])
            out.mkdir(parents=True, exist_ok=True)
            (out / "receipt.json").write_text(json.dumps({"ratchet": ratchet or {}}))
        return subprocess.CompletedProcess(args, 0, "", "")

    @contextlib.contextmanager
    def lock():
        if inside_lock:
            inside_lock()
        yield

    def push(main, head):
        pushes.append(head)
        return len(pushes) > 1  # the first push loses to a moving main

    def execv(*args):
        raise Restart(args)

    cwd = os.getcwd()
    os.chdir(repos.candidate)
    raised = None
    try:
        with patch.object(land, "run", run), \
                patch.object(land, "apply_format_witnesses", lambda *a: []), \
                patch.object(land, "land_lock", lock), \
                patch.object(land, "push", push), \
                patch.object(land.os, "execv", execv), \
                patch.object(sys, "argv", ["scripts/land"]), \
                patch.dict(os.environ, {"GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t",
                                        "GIT_COMMITTER_NAME": "t", "GIT_COMMITTER_EMAIL": "t@t",
                                        "LASH_BUCK2_STORE": str(repos.other.parent / "buck2-store")}), \
                contextlib.redirect_stdout(io.StringIO()):
            os.environ.pop("LASH_LAND_RUNS", None)
            try:
                land.main()
            except (Restart, RuntimeError, ValueError) as error:
                raised = error
    finally:
        os.chdir(cwd)
    return land, qualified, pushes, raised


class LandInProcess(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.repos = Repositories(Path(directory.name))

    def test_a_push_lost_to_a_moving_main_restarts_instead_of_rebasing_in_process(self):
        """A push lost to a moving main restarts the land, so the next rebase
        is again the first of a process and is checked for changed land code."""
        _, _, pushes, raised = land_in_process(self.repos)
        self.assertIsInstance(raised, Restart)
        self.assertEqual(len(pushes), 1)

    def test_a_head_changed_after_qualification_is_never_pushed(self):
        """Job I1: the land pushes the head it qualified, or its own verified
        ceiling commit on it; a commit made after qualification is refused,
        and left in place."""
        candidate = self.repos.candidate

        def commit_changed_land():
            (candidate / "scripts/land").write_text("raise SystemExit('unqualified land code')\n")
            git(candidate, "add", "scripts/land")
            git(candidate, "commit", "-qm", "unqualified change after the gate")

        _, qualified, pushes, raised = land_in_process(self.repos, inside_lock=commit_changed_land)
        self.assertEqual(len(qualified), 1)
        self.assertEqual(pushes, [], f"an unqualified head reached push; qualified {qualified[0]}")
        self.assertIn("changed after qualification", str(raised))
        self.assertEqual(git(candidate, "log", "-1", "--format=%s"), "unqualified change after the gate")

    def test_the_land_ceiling_commit_only_lowers_ceilings(self):
        """Job I2: what the land commits is exactly the qualified policy with
        finite, positive decreases. A receipt that raises, keeps or poisons
        a ceiling is refused, and so is a commit hook that rewrites the
        generated commit; nothing reaches push and the head returns to the
        qualified one."""
        candidate = self.repos.candidate
        ceilings = candidate / "scripts/perf_land_ceilings.py"
        key = "typescript/int_arith"
        old = policy_values(ceilings.read_text())["ROWS"][key]["ceiling"]
        hook = ("#!/usr/bin/env python3\nfrom pathlib import Path\nimport subprocess\n"
                "p = Path('scripts/perf_land_ceilings.py')\n"
                f"p.write_text(p.read_text().replace({repr(old * .9)!r}, {repr(old * 2)!r}))\n"
                "subprocess.run(['git', 'add', str(p)], check=True)\n")
        cases = {"raised by the receipt": ({key: old * 2}, None),
                 "kept by the receipt": ({key: old}, None),
                 "not finite": ({key: float("nan")}, None),
                 "not positive": ({key: 0.0}, None),
                 "raised by a commit hook": ({key: old * .9}, hook)}
        for name, (ratchet, hook_source) in cases.items():
            with self.subTest(name):
                hooks = candidate / ".git/hooks/pre-commit"
                hooks.unlink(missing_ok=True)
                if hook_source:
                    hooks.write_text(hook_source)
                    hooks.chmod(0o755)
                _, qualified, pushes, raised = land_in_process(self.repos, ratchet=ratchet)
                self.assertEqual(pushes, [], f"a ceiling commit {name} reached push")
                self.assertIsNotNone(raised)
                self.assertNotIsInstance(raised, Restart)
                self.assertEqual(git(candidate, "rev-parse", "HEAD"), qualified[0])
                self.assertEqual(policy_values(ceilings.read_text())["ROWS"][key]["ceiling"], old)

if __name__ == "__main__":
    unittest.main()
