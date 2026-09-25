#!/usr/bin/env python3
"""Plan and serialize change-scoped validation for a checkout/config snapshot."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import shlex
import subprocess
import sys
import time
import xml.etree.ElementTree as ElementTree


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
# `LASH_QUICK` (AGENTS.md): the opt-in iteration knob for the heavy lanes.
QUICK = "LASH_QUICK"
# Comma-separated shards/paths the quick test262 selection keeps whole.
QUICK_TEST262_INCLUDE = "LASH_TEST262_QUICK_INCLUDE"
TEST262_PREFIX = "crates/lash-typescript/tests/test262/"
SUMMARY_LINES = 40


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


def quick_enabled() -> bool:
    value = os.environ.get(QUICK, "")
    return bool(value) and value != "0"


def quick_test262_includes(paths: list[str]) -> set[str]:
    """The test262 shards a changed file asks the quick subset to keep whole.

    The quick selection samples ~10% of each stratum; a diff under
    `test/<shard>/` or `outcomes/<shard>.tsv` keeps that shard's tests in the
    run so the change actually exercises the area it edits. Any other test262
    input (census.tsv, the harness, the shared runner) is selection-wide, so
    it keeps every shard whole. The Rust side resolves shard names and `*`.
    """
    includes = set()
    for path in paths:
        if not path.startswith(TEST262_PREFIX):
            continue
        rest = path[len(TEST262_PREFIX):].split("/")
        if rest[0] == "test" and len(rest) > 2:
            includes.add(rest[1])
        elif rest[0] == "outcomes" and rest[-1].endswith(".tsv"):
            includes.add(rest[-1].removesuffix(".tsv"))
        else:
            includes.add("*")
    # A selection-wide change keeps every shard; shard names add nothing.
    return {"*"} if "*" in includes else includes


def quick_test_args(paths: list[str]) -> list[str]:
    """The `--test_env` flags that carry LASH_QUICK into remote test actions.

    Emitting the env only when the knob is set keeps a full run's action keys
    identical to CI's, and a quick run's cached verdicts useless to a full
    run. CI never sets the knob; the full selection stays the gate.
    """
    if not quick_enabled():
        return []
    args = [f"--test_env={QUICK}=1"]
    includes = sorted(quick_test262_includes(paths))
    if includes:
        args.append(f"--test_env={QUICK_TEST262_INCLUDE}={','.join(includes)}")
    return args


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
        commands.append(["kiln", "test", *quick_test_args(paths), *labels])
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


# Bazel's summary block: `//pkg:tgt  FAILED in 1.2s` followed by indented
# artifact paths, and `FAIL: //pkg:tgt (see /path/test.log)` lines.
_TARGET_STATUS = re.compile(
    r"^(\s*)(//\S+)\s+.*\b(FAILED|TIMEOUT|FLAKY|NO STATUS)\b", re.MULTILINE)
_SEE = re.compile(r"^FAIL:\s+(//\S+)\s+\((?:see|cached)\s+(\S+)\)", re.MULTILINE)


def failed_targets(text: str) -> list[tuple[str, list[str]]]:
    """Each failed Bazel label with the artifact paths printed beneath it."""
    lines = text.splitlines()
    targets: list[tuple[str, list[str]]] = []
    for index, line in enumerate(lines):
        label, artifacts = None, []
        if match := _TARGET_STATUS.match(line):
            label = match.group(2)
        elif match := _SEE.match(line):
            label, artifacts = match.group(1), [match.group(2)]
        if label is None:
            continue
        follow = index + 1
        while follow < len(lines) and (line := lines[follow]).startswith((" ", "\t")) and line.strip():
            artifacts += [t for t in line.split() if "testlogs/" in t or t.endswith(("test.log", "test.xml"))]
            follow += 1
        targets.append((label, artifacts))
    return targets


def panic_line(text: str) -> str | None:
    """The first Rust panic as `file:line: message`, else an assertion line."""
    lines = text.splitlines()
    for index, line in enumerate(lines):
        if "panicked at" not in line:
            continue
        location = re.search(r"panicked at (\S+?:\d+:\d+)", line)
        message = next((follow.strip() for follow in lines[index + 1:] if follow.strip()), "")
        return f"{location.group(1)}: {message}" if location else line.strip()
    for line in lines:
        if re.search(r"AssertionError|assertion .* failed|assertion failed|assert_?eq!|assert_?ne!", line):
            return line.strip()
    return None


def _libtest_names(text: str) -> list[str]:
    names = re.findall(r"^test (\S+) \.\.\. FAILED\b", text, re.MULTILINE)
    for block in re.findall(r"^failures:\s*\n((?:[ \t]+\S.*\n)+)", text, re.MULTILINE):
        names += re.findall(r"^\s+(\S+)", block, re.MULTILINE)
    return list(dict.fromkeys(names))


def _xml_failures(path: Path) -> tuple[list[str], str | None]:
    try:
        root = ElementTree.parse(path).getroot()
    except (OSError, ElementTree.ParseError):
        return [], None
    names, detail = [], None
    for case in root.iter("testcase"):
        node = next((case.find(kind) for kind in ("failure", "error") if case.find(kind) is not None), None)
        if node is None:
            continue
        names.append(case.get("name") or "?")
        if detail is None:
            detail = panic_line(node.text or "") or (node.get("message") or "").strip() or None
    return names, detail


def target_failure(artifacts: list[str]) -> tuple[list[str], str | None, str | None]:
    """Failing test names, first panic line, and log path for one target."""
    log = next((a for a in artifacts if a.endswith("test.log")), None)
    xml = next((a for a in artifacts if a.endswith("test.xml")), None)
    if xml is None and log:
        xml = log.removesuffix("test.log") + "test.xml"
    names, detail = [], None
    if xml and Path(xml).is_file():
        names, detail = _xml_failures(Path(xml))
    if not names and log and Path(log).is_file():
        text = Path(log).read_text(errors="replace")
        names, detail = _libtest_names(text), panic_line(text)
    return names, detail, log


def failure_summary(command: list[str], code: int, log_path: Path) -> str:
    """The ~40-line digest printed when a validation command fails."""
    try:
        text = log_path.read_text(errors="replace")
    except OSError:
        text = ""
    lines = [
        f"dev-test: `{shlex.join(command)}` failed with exit {code}",
        f"dev-test: full output: {log_path}",
    ]
    targets = failed_targets(text)
    if targets:
        lines.append("dev-test: failing targets:")
        for label, artifacts in targets[:12]:
            names, detail, log = target_failure(artifacts)
            lines.append(f"  {label}")
            if names:
                shown = ", ".join(names[:8])
                lines.append(f"    failing tests: {shown}{f' (+{len(names) - 8} more)' if len(names) > 8 else ''}")
            if detail:
                lines.append(f"    {detail}")
            if log:
                lines.append(f"    log: {log}")
        if len(targets) > 12:
            lines.append(f"  ... and {len(targets) - 12} more failing targets")
    else:
        lines.append("dev-test: last output lines:")
        tail = [line for line in text.splitlines() if line.strip()][-16:]
        lines += [f"  {line}" for line in tail]
    lines.append("dev-test: rerun with --verbose to stream full output")
    return "\n".join(lines[:SUMMARY_LINES])


def run(planned: dict, verbose: bool) -> int:
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
            for index, command in enumerate(planned["commands"]):
                print("+ " + shlex.join(command), flush=True)
                log_path = directory / f"command-{index}.log"
                if verbose:
                    process = subprocess.Popen(command, cwd=ROOT, start_new_session=True)
                else:
                    # Command output goes to a per-command log; a failure
                    # prints the digest of it, not the whole log.
                    with log_path.open("wb") as sink:
                        process = subprocess.Popen(
                            command, cwd=ROOT, start_new_session=True,
                            stdout=sink, stderr=subprocess.STDOUT)
                code = process.wait()
                if code:
                    if not verbose:
                        print(failure_summary(command, code, log_path))
                    break
                if not verbose:
                    print(f"dev-test: exit 0, output {log_path}", flush=True)
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
    parser.add_argument("--verbose", action="store_true", help="Stream each command's output instead of logging it and summarizing failures")
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
    return run(planned, args.verbose)


if __name__ == "__main__":
    sys.exit(main())
