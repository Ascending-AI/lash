#!/usr/bin/env python3
"""Plan and serialize change-scoped validation for a checkout/config snapshot."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import shlex
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
# No bytecode beside the classifier: an untracked `__pycache__` is a changed
# path to the very selection this import makes.
sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT / "scripts"))
import ci_plan  # noqa: E402
LIVE_STORES = (
    "LASH_POSTGRES_DATABASE_URL", "LASH_REQUIRE_POSTGRES", "LASH_S3_ENDPOINT",
    "LASH_REQUIRE_S3",
)


def git(*args: str) -> bytes:
    return subprocess.check_output(["git", *args], cwd=ROOT)


def changed_files(base: str) -> list[str]:
    return sorted({
        os.fsdecode(path)
        for output in (
            git("diff", "--name-only", "--no-renames", "-z", base, "HEAD"),
            git("diff", "--name-only", "--no-renames", "-z", "HEAD"),
            git("ls-files", "--others", "--exclude-standard", "-z"),
        )
        for path in output.split(b"\0") if path
    })


def input_id(base: str) -> str:
    digest = hashlib.sha256()

    def add(value: bytes) -> None:
        digest.update(len(value).to_bytes(8, "big"))
        digest.update(value)

    add(base.encode())
    add(git("rev-parse", "HEAD"))
    add(git("diff", "--binary", "--no-ext-diff", "HEAD"))
    add(json.dumps({
        name: value for name, value in os.environ.items()
        if name.startswith(("LASH_", "KILN_", "BAZEL_", "RUST", "CARGO_"))
        or name == "PATH"
    }, sort_keys=True).encode())
    # These ignored files select the actual executor/tooling of a fork.
    paths = set(git("ls-files", "--others", "--exclude-standard", "-z").split(b"\0"))
    paths.update((b".kiln.bazelrc", b"env.sh"))
    for raw in sorted(paths - {b""}):
        path = ROOT / os.fsdecode(raw)
        add(raw)
        if path.is_symlink():
            add(b"symlink:" + os.fsencode(os.readlink(path)))
        elif path.is_file():
            add(str(path.stat().st_mode).encode())
            add(path.read_bytes())
        else:
            add(b"missing")
    return digest.hexdigest()


def script_gates() -> dict[str, list[list[str]]]:
    """Read the executable script inventory shared by CI and repository-gates."""
    commands: dict[str, list[list[str]]] = {}
    capture = False
    for line in (ROOT / ".github/workflows/ci.yml").read_text().splitlines():
        if "run-gate-commands.sh " in line and "<<'GATES'" in line:
            capture = True
        elif capture and line.strip() == "GATES":
            capture = False
        elif capture and line.strip():
            command = shlex.split(line.strip())
            if len(command) >= 2 and command[0] == "python3":
                commands.setdefault(command[1], []).append(command)
    if not commands:
        raise RuntimeError("CI repository gate inventory is empty")
    return commands


def select(paths: list[str], gates: dict[str, list[list[str]]]) -> tuple[list[str], bool, bool, list[list[str]]]:
    # `scripts/ci_plan.py` is the repository's one change classifier; this is
    # its dev-test projection plus the commands each part of it runs.
    scope = ci_plan.dev_test_scope(paths, ROOT, frozenset(gates))
    commands = ([["bash", "scripts/ci/repository-gates.sh"]] if scope.repository else
                [command for name in scope.script_tests for command in gates[name]])
    return list(scope.packages), scope.broad, scope.facade, commands


def plan(base: str, dependents: bool) -> dict:
    identity = input_id(base)
    paths = changed_files(base)
    packages, broad, facade, commands = select(paths, script_gates())
    allowed, batches = ci_plan.dev_test_inventory(ROOT)
    members = {label for label in allowed if label.split(":")[0] in packages}
    if dependents and packages and not broad:
        # Restrict the query universe to first-party packages. `//...` also
        # traverses Bazel's generated bazel-src symlink in a fork and can fail
        # while loading its external repository aliases, forcing a broad suite.
        expression = 'kind("test", rdeps(set(//crates/... //examples/... //runbooks/...), set(' + " ".join(p + ":all" for p in packages) + ')))'
        result = subprocess.run(
            ["bazel", "query", expression], cwd=ROOT, capture_output=True, text=True
        )
        if result.returncode:
            print(result.stderr, file=sys.stderr)
            broad = True
        else:
            members = set(result.stdout.split()) & allowed
    # A change under a package's directory also runs that package's
    # `dev-deferred` labels: the tail leg is merge-group-only in CI, so a
    # change to a deferred test's inputs (#2109's corpus expectations file)
    # otherwise lands untested. This holds on a broad plan too -- a package
    # manifest widens the selection but is still a deferred test's input.
    # The label assembly is ci_plan.affected_bazel_labels: the same selection
    # the pull-request leg of `bazel-tests` runs in CI.
    tail = ci_plan.pr_tail_labels(paths, ROOT)
    scope = ci_plan.DevTestScope(tuple(sorted(packages)), broad, facade, False, ())
    labels, builds = ci_plan.affected_bazel_labels(scope, members, tail, batches)
    if builds:
        commands.append(["kiln", "build", *builds])
    if labels:
        commands.append(["kiln", "test", *labels])
    result = {
        "base": base,
        "head": git("rev-parse", "HEAD").decode().strip(),
        "inputs": identity,
        "identity_scope": "checkout/config snapshot; Bazel validates full action inputs",
        "changed_files": paths,
        "selection": "suite" if broad else "dependents" if dependents else "packages",
        "commands": commands,
        "remaining": ["Required CI gates; this is focused local validation, not full CI"],
    }
    result["id"] = hashlib.sha256(json.dumps(result, sort_keys=True).encode()).hexdigest()
    if input_id(base) != identity:
        raise RuntimeError("checkout/config snapshot changed while selecting tests; rerun")
    return result


def save(path: Path, value: dict) -> None:
    temporary = path.with_suffix(f".{os.getpid()}.tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def run(planned: dict) -> int:
    directory = Path(git("rev-parse", "--path-format=absolute", "--git-path", "lash-validation").decode().strip())
    directory.mkdir(mode=0o700, exist_ok=True)
    with (directory / "lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("dev-test: waiting for this fork's running validation", flush=True)
            fcntl.flock(lock, fcntl.LOCK_EX)
        if input_id(planned["base"]) != planned["inputs"]:
            print("dev-test: checkout/config snapshot changed while planning/waiting; rerun", file=sys.stderr)
            return 2
        receipt_path = directory / "latest.json"
        # Only Bazel knows the full declared action inputs, including ignored
        # package data and external toolchains. A prior receipt cannot replace
        # its cache validation, even for an overlapping identical request.
        started = time.time_ns()
        save(directory / "plan.json", planned)
        print(f"dev-test: checkout/config snapshot {planned['inputs'][:12]}, {planned['selection']}", flush=True)
        process = None
        try:
            code = 0
            for command in planned["commands"]:
                print("+ " + shlex.join(command), flush=True)
                process = subprocess.Popen(command, cwd=ROOT, start_new_session=True)
                code = process.wait()
                if code:
                    break
        except KeyboardInterrupt:
            if process is not None and process.poll() is None:
                os.killpg(process.pid, signal.SIGTERM)
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
            code = 130
        except OSError as error:
            print(f"dev-test: {error}", file=sys.stderr)
            code = 2
        unchanged = input_id(planned["base"]) == planned["inputs"]
        if not unchanged:
            print("dev-test: checkout/config snapshot changed during validation; result is stale", file=sys.stderr)
            code = 2
        receipt = {
            "plan": planned, "started_ns": started, "finished_ns": time.time_ns(),
            "exit_code": max(code, 0) if code >= 0 else 128 - code,
            "inputs_unchanged": unchanged,
        }
        save(receipt_path, receipt)
        print(f"dev-test: receipt {receipt_path}")
        return receipt["exit_code"]


def main() -> int:
    def interrupt(_signal, _frame):
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, interrupt)
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--base", help="Comparison revision; defaults to merge-base with origin/main")
    parser.add_argument("--dependents", action="store_true", help="Include Bazel reverse dependencies")
    parser.add_argument("--dry-run", action="store_true", help="Print the exact plan as JSON without executing")
    args = parser.parse_args()
    if any(os.environ.get(name) for name in LIVE_STORES):
        parser.error("live store URLs are not accepted; CI owns Postgres/S3/E2E")
    if not (ROOT / ".kiln.bazelrc").is_file():
        parser.error("run inside a Kiln fork")
    if args.base:
        base = git("rev-parse", "--verify", args.base + "^{commit}").decode().strip()
    else:
        base = git("merge-base", "HEAD", "origin/main").decode().strip()
    try:
        planned = plan(base, args.dependents)
    except (RuntimeError, KeyError, ValueError, OSError) as error:
        parser.error(str(error))
    if args.dry_run:
        print(json.dumps(planned, indent=2))
        return 0
    return run(planned)


if __name__ == "__main__":
    sys.exit(main())
