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


def land_in_process(repos, ratchet=None, at=None, lost_push=False):
    """Run this tree's scripts/land main() against the fixture repositories,
    with qualification stubbed: every git operation, commit, hook and push to
    the fixture origin is real. The perf gate's receipt carries `ratchet`.
    `at` maps a moment to a callable run there: "qualified" (after the
    gate), "committed" (after the land's commit), "lock" (entering the land
    lock), "push" (before git push) and "cleanup" (before the land rewrites
    the branch after a refusal). `lost_push` replaces push with one that
    loses to a moving main. Answers the qualified heads, the push sources,
    what main() raised, and the output."""
    at = at or {}
    loader = importlib.machinery.SourceFileLoader("land_under_test", str(SCRIPTS / "land"))
    spec = importlib.util.spec_from_loader(loader.name, loader)
    land = importlib.util.module_from_spec(spec)
    loader.exec_module(land)
    real_run = land.run
    qualified, pushes = [], []

    def run(*args, check=True, quiet=False):
        if args[0] == "git":
            if args[1] == "push":
                pushes.append(args[-1].split(":")[0])
                at.get("push", lambda: None)()
            if args[1] in ("reset", "update-ref"):
                at.get("cleanup", lambda: None)()
            result = real_run(*args, check=check, quiet=quiet)
            if args[1] == "commit":
                at.get("committed", lambda: None)()
            return result
        if "--out" in args:  # the perf gate: a qualified receipt
            qualified.append(land.git("rev-parse", "HEAD"))
            out = Path(args[args.index("--out") + 1])
            out.mkdir(parents=True, exist_ok=True)
            (out / "receipt.json").write_text(json.dumps({"ratchet": ratchet or {}}))
            at.get("qualified", lambda: None)()
        return subprocess.CompletedProcess(args, 0, "", "")

    @contextlib.contextmanager
    def lock():
        at.get("lock", lambda: None)()
        yield

    def lose(main, head):
        pushes.append(head)
        return False

    def execv(*args):
        raise Restart(args)

    cwd = os.getcwd()
    os.chdir(repos.candidate)
    raised = None
    output = io.StringIO()
    try:
        with contextlib.ExitStack() as stack:
            stack.enter_context(patch.object(land, "run", run))
            stack.enter_context(patch.object(land, "apply_format_witnesses", lambda *a: []))
            stack.enter_context(patch.object(land, "land_lock", lock))
            if lost_push:
                stack.enter_context(patch.object(land, "push", lose))
            stack.enter_context(patch.object(land.os, "execv", execv))
            stack.enter_context(patch.object(sys, "argv", ["scripts/land"]))
            stack.enter_context(patch.dict(os.environ, {
                "GIT_AUTHOR_NAME": "t", "GIT_AUTHOR_EMAIL": "t@t", "GIT_COMMITTER_NAME": "t",
                "GIT_COMMITTER_EMAIL": "t@t", "LASH_BUCK2_STORE": str(repos.other.parent / "buck2-store")}))
            stack.enter_context(contextlib.redirect_stdout(output))
            os.environ.pop("LASH_LAND_RUNS", None)
            try:
                land.main()
            except (Restart, RuntimeError, ValueError) as error:
                raised = error
    finally:
        os.chdir(cwd)
    return qualified, pushes, raised, output.getvalue()


def foreign_commit(repo, made, text="foreign work\n"):
    """Commit someone else's work on the checked-out branch."""
    (repo / "foreign.txt").write_text(text)
    git(repo, "add", "foreign.txt")
    git(repo, "commit", "-qm", "foreign work")
    made.append(git(repo, "rev-parse", "HEAD"))


def shadow(repo, made, ceilings_raised_to=None):
    """Point a branch named after the object HEAD names at an unqualified
    commit, without moving HEAD: push must still send HEAD's object."""
    head = git(repo, "rev-parse", "HEAD")
    if ceilings_raised_to:
        path = repo / "scripts/perf_land_ceilings.py"
        old, new = ceilings_raised_to
        path.write_text(path.read_text().replace(repr(old), repr(new)))
        git(repo, "add", "scripts/perf_land_ceilings.py")
    else:
        (repo / "foreign.txt").write_text("unqualified ref-only work\n")
        git(repo, "add", "foreign.txt")
    tree = git(repo, "write-tree")
    git(repo, "reset", "-q", "--hard", head)
    made.append(git(repo, "commit-tree", tree, "-p", head, "-m", "unqualified ref-only work"))
    git(repo, "update-ref", "refs/heads/" + head, made[-1])


class LandInProcess(unittest.TestCase):
    def setUp(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        self.repos = Repositories(Path(directory.name))

    def fresh(self):
        directory = tempfile.TemporaryDirectory()
        self.addCleanup(directory.cleanup)
        return Repositories(Path(directory.name))

    def test_a_push_lost_to_a_moving_main_restarts_instead_of_rebasing_in_process(self):
        """A push lost to a moving main restarts the land, so the next rebase
        is again the first of a process and is checked for changed land code."""
        _, pushes, raised, _ = land_in_process(self.repos, lost_push=True)
        self.assertIsInstance(raised, Restart)
        self.assertEqual(len(pushes), 1)

    def test_a_head_changed_after_qualification_is_never_pushed(self):
        """Jobs I and I2: main receives exactly the qualified head, or the
        land's own verified ceiling commit on it, whatever ref names it. A
        refused land removes only a commit it can prove it made, by a
        compare-and-swap on the branch, and leaves anyone else's work."""
        key = "typescript/int_arith"

        def changed_land(repos, made):
            (repos.candidate / "scripts/land").write_text("raise SystemExit('unqualified land code')\n")
            git(repos.candidate, "add", "scripts/land")
            git(repos.candidate, "commit", "-qm", "unqualified change after the gate")
            made.append(git(repos.candidate, "rev-parse", "HEAD"))

        refused = {  # moment -> what is done there; the land must refuse
            "a changed scripts/land committed inside the lock": (None, "lock", changed_land),
            "foreign work committed after qualification": (.9, "qualified", None),
            "foreign work committed on the land's ceiling commit": (.9, "committed", None),
            "foreign work committed as the refused land cleans up": (.9, "cleanup", None),
        }
        for name, (factor, moment, act) in refused.items():
            with self.subTest(name):
                repos = self.fresh()
                main = git(repos.origin, "rev-parse", "main")
                old = policy_values((repos.candidate / "scripts/perf_land_ceilings.py").read_text())["ROWS"][key]["ceiling"]
                made = []
                at = {moment: lambda: (act or (lambda r, m: foreign_commit(r.candidate, m)))(repos, made)}
                if moment == "cleanup":  # a refusal after the land's own verified commit
                    at["lock"] = lambda: (_ for _ in ()).throw(RuntimeError("refused inside the lock"))
                qualified, pushes, raised, output = land_in_process(
                    repos, ratchet={key: old * factor} if factor else None, at=at)
                self.assertIsNotNone(raised)
                self.assertNotIsInstance(raised, Restart)
                self.assertEqual(git(repos.origin, "rev-parse", "main"), main, "main moved")
                self.assertEqual(len(made), 1)
                self.assertEqual(git(repos.candidate, "rev-parse", "HEAD"), made[0],
                                 f"{name}: the refused land moved the branch off the work left on it")

        with self.subTest("a branch named after the qualified head at push"):
            repos = self.fresh()
            made = []
            qualified, _, raised, _ = land_in_process(repos, at={"push": lambda: shadow(repos.candidate, made)})
            self.assertIsNone(raised)
            self.assertEqual(git(repos.origin, "rev-parse", "main"), qualified[0])

    def test_the_land_ceiling_commit_only_lowers_ceilings(self):
        """Jobs I and I2: what main receives from the land's ceiling commit
        is exactly the qualified policy with finite, positive decreases. A
        receipt that raises, keeps or poisons a ceiling is refused, and so is
        a commit hook that rewrites the generated commit; nothing reaches
        main and the head returns to the qualified one. A branch named after
        the ceiling commit does not redirect its push."""
        key = "typescript/int_arith"
        old = policy_values((self.repos.candidate / "scripts/perf_land_ceilings.py").read_text())["ROWS"][key]["ceiling"]
        hook = ("#!/usr/bin/env python3\nfrom pathlib import Path\nimport subprocess\n"
                "p = Path('scripts/perf_land_ceilings.py')\n"
                f"p.write_text(p.read_text().replace({repr(old * .9)!r}, {repr(old * 2)!r}))\n"
                "subprocess.run(['git', 'add', str(p)], check=True)\n")
        def rewrite_unchanged(ceilings):  # a stat-only change the index has not seen
            ceilings.write_bytes(ceilings.read_bytes())
            os.utime(ceilings, (1, 1))

        cases = {"raised by the receipt": ({key: old * 2}, None, None),
                 "kept by the receipt": ({key: old}, None, None),
                 "not finite": ({key: float("nan")}, None, None),
                 "not positive": ({key: 0.0}, None, None),
                 "raised by a commit hook": ({key: old * .9}, hook, None),
                 "raised by a commit hook, the file rewritten unchanged at cleanup":
                     ({key: old * .9}, hook, rewrite_unchanged)}
        for name, (ratchet, hook_source, at_cleanup) in cases.items():
            with self.subTest(name):
                repos = self.fresh()
                main = git(repos.origin, "rev-parse", "main")
                ceilings = repos.candidate / "scripts/perf_land_ceilings.py"
                if hook_source:
                    hooks = repos.candidate / ".git/hooks/pre-commit"
                    hooks.write_text(hook_source)
                    hooks.chmod(0o755)
                qualified, pushes, raised, _ = land_in_process(
                    repos, ratchet=ratchet, at={"cleanup": lambda: at_cleanup(ceilings)} if at_cleanup else None)
                self.assertEqual(pushes, [], f"a ceiling commit {name} reached push")
                self.assertEqual(git(repos.origin, "rev-parse", "main"), main)
                self.assertIsNotNone(raised)
                self.assertNotIsInstance(raised, Restart)
                self.assertEqual(git(repos.candidate, "rev-parse", "HEAD"), qualified[0])
                self.assertEqual(policy_values(ceilings.read_text())["ROWS"][key]["ceiling"], old)

        with self.subTest("a branch named after the ceiling commit at push"):
            repos = self.fresh()
            made = []
            qualified, _, raised, _ = land_in_process(
                repos, ratchet={key: old * .9},
                at={"push": lambda: shadow(repos.candidate, made, (old * .9, old * 2))})
            self.assertIsNone(raised)
            landed = git(repos.origin, "rev-parse", "main")
            self.assertEqual(git(repos.origin, "rev-parse", landed + "^"), qualified[0])
            self.assertEqual(policy_values(git(repos.origin, "show", landed + ":scripts/perf_land_ceilings.py"))
                             ["ROWS"][key]["ceiling"], old * .9)


if __name__ == "__main__":
    unittest.main()
