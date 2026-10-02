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
import threading
import time
import xml.etree.ElementTree as ElementTree


ROOT = Path(__file__).resolve().parents[1]
# No bytecode beside the classifier: an untracked `__pycache__` is a changed
# path to the very selection this import makes.
sys.dont_write_bytecode = True
sys.path.insert(0, str(ROOT / "scripts"))
sys.path.insert(0, str(ROOT / "scripts/ci"))
import ci_plan  # noqa: E402
import repository_gate_commands  # noqa: E402
LIVE_STORES = (
    "LASH_POSTGRES_DATABASE_URL", "LASH_REQUIRE_POSTGRES", "LASH_S3_ENDPOINT",
    "LASH_REQUIRE_S3",
)
# `LASH_QUICK`: the opt-in iteration knob for the heavy lanes.
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


def snapshot_contents(path: os.PathLike[str] | str | bytes, contents: bytes) -> bytes:
    """Exclude the driver's startup-only remote concurrency setting."""
    if os.fsdecode(path) != ".buckconfig.local":
        return contents
    try:
        lines = contents.decode().splitlines(keepends=True)
    except UnicodeDecodeError:
        return contents
    section = None
    kept = []
    for line in lines:
        match = re.fullmatch(r"\s*\[([^]]+)]\s*(?:[#;].*)?\r?\n?", line)
        if match:
            section = match[1]
        if section == "buck2_re_client" and re.match(
            r"\s*execution_concurrency_limit\s*=", line
        ):
            continue
        kept.append(line)
    return "".join(kept).encode()


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
        if name.startswith(("LASH_", "KILN_", "BUCK2_", "RUST", "CARGO_"))
        or name == "PATH"
    }, sort_keys=True).encode())
    # These ignored files select the actual executor/tooling of a fork.
    paths = set(git("ls-files", "--others", "--exclude-standard", "-z").split(b"\0"))
    paths.update((b".buckconfig.local", b"env.sh"))
    for raw in sorted(paths - {b""}):
        path = ROOT / os.fsdecode(raw)
        add(raw)
        if path.is_symlink():
            add(b"symlink:" + os.fsencode(os.readlink(path)))
        elif path.is_file():
            add(str(path.stat().st_mode).encode())
            add(snapshot_contents(raw, path.read_bytes()))
        else:
            add(b"missing")
    return digest.hexdigest()


def script_gates() -> dict[str, list[list[str]]]:
    """Read the executable script inventory shared by CI and repository-gates:
    the checks CI lists and the self-tests both entrypoints discover."""
    lines = []
    capture = False
    for line in (ROOT / ".github/workflows/ci.yml").read_text().splitlines():
        if "run-gate-commands.sh " in line and "<<'GATES'" in line:
            capture = True
        elif capture and line.strip() == "GATES":
            capture = False
        elif capture and line.strip() and not line.strip().startswith("#"):
            lines.append(line.strip())
    commands: dict[str, list[list[str]]] = {}
    for line in [*lines, *repository_gate_commands.commands(ROOT)]:
        command = shlex.split(line)
        if len(command) >= 2 and command[0] == "python3":
            listed = commands.setdefault(command[1], [])
            # The runner executes a command both sources name only once.
            if command not in listed:
                listed.append(command)
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


def root_cell_label(label: str) -> str:
    """Return Buck2's root-cell label in the inventory's `//package:target` form."""
    return label.removeprefix("root") if label.startswith("root//") else label


def text_at(base: str):
    """Read a path's text at `base`: what the precise selection diffs against."""
    def read(path: str) -> str | None:
        result = subprocess.run(
            ["git", "show", f"{base}:{path}"], cwd=ROOT, capture_output=True)
        return result.stdout.decode(errors="replace") if result.returncode == 0 else None
    return read


def select(paths: list[str], gates: dict[str, list[list[str]]], base_text=None) -> tuple[ci_plan.DevTestScope, list[list[str]]]:
    # `scripts/ci_plan.py` is the repository's one change classifier; this is
    # its dev-test projection plus the commands each part of it runs.
    scope = ci_plan.dev_test_scope(paths, ROOT, frozenset(gates), base_text)
    if scope.repository:
        # Leave out the gates the change provably does not reach.
        inventory = [command for commands in gates.values() for command in commands]
        skips = [gate for gate in ci_plan.unaffected_repository_gates(paths)
                 if shlex.split(gate) in inventory]
        commands = [["bash", "scripts/ci/repository-gates.sh",
                     *(part for gate in skips for part in ("--skip", gate))]]
    else:
        commands = [command for name in scope.script_tests for command in gates[name]]
    return scope, commands


def buck2_client() -> list[str] | None:
    """The bootstrapped Buck2 client on the daemon `kiln test` uses."""
    bootstrap = subprocess.run(
        [sys.executable, "tools/buck2/bootstrap.py"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    client = bootstrap.stdout.strip()
    if bootstrap.returncode or not client:
        print(bootstrap.stderr, file=sys.stderr)
        return None
    return [client, "--isolation-dir", os.environ.get("BUCK_ISOLATION_DIR", "kiln")]


def file_owners(files: list[str]) -> dict[str, list[str]] | None:
    """The targets that declare each file as an input, or None if Buck2 cannot say."""
    client = buck2_client()
    # `owner` loads only the owning package, so the unconfigured query never
    # reaches the `select` branches that `reverse_dependencies` avoids.
    result = subprocess.run(
        [*client, "uquery", "--json", "owner('%s')", *files] if client else ["false"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    try:
        owners = json.loads(result.stdout) if result.returncode == 0 else None
    except ValueError:
        owners = None
    if not isinstance(owners, dict):
        print(result.stderr, file=sys.stderr)
        return None
    return {
        path: [root_cell_label(label) for label in owners.get(path) or []]
        for path in files
    }


def reverse_dependencies(seeds: list[str]) -> set[str] | None:
    """Every test that depends on a seed target, or None if Buck2 cannot say.

    A seed is a package pattern (`//package:`) or one label.
    """
    # Restrict the query universe to first-party packages so generated and
    # third-party cells cannot widen the selection.
    expression = 'kind("test", rdeps(set(//crates/... //examples/... //runbooks/...), set(' + " ".join(seeds) + ')))'
    client = buck2_client()
    # A configured query against the daemon and configuration `kiln test`
    # uses. `uquery` follows every `select` branch, including the prelude's
    # `toolchains//:cxx_no_default_deps`, which this graph does not define.
    result = subprocess.run(
        [*client, "cquery", "-c", "kiln.execution_mode=remote", expression]
        if client else ["false"],
        cwd=ROOT,
        capture_output=True,
        text=True,
    )
    if result.returncode:
        print(result.stderr, file=sys.stderr)
        print("dev-test: the reverse-dependency query failed; selecting the whole suite", file=sys.stderr)
        return None
    # A configured label prints as `root//package:name (configuration)`.
    return {root_cell_label(line.split()[0]) for line in result.stdout.splitlines() if line.strip()}


def plan(base: str, dependents: bool, include_deferred: bool = False) -> dict:
    identity = input_id(base)
    paths = changed_files(base)
    # Only a run that asks Buck2 for reverse dependencies reads what a shared
    # input's change is; package iteration keeps the whole-suite widening.
    scope, commands = select(paths, script_gates(), text_at(base) if dependents else None)
    packages, broad, facade = list(scope.packages), scope.broad, scope.facade
    allowed, batches = ci_plan.dev_test_inventory(ROOT)
    members = {label for label in allowed if label.split(":")[0] in packages}
    deferred: list[str] = []
    skipped: list[str] = []
    seeds = [package + ":" for package in packages] + list(scope.targets)
    if dependents and (scope.files or scope.machinery) and not broad:
        owners = file_owners([*scope.files, *scope.machinery])
        # A query that failed owns nothing, so it widens like an unowned file.
        unowned = sorted(
            path for path in ([*scope.files, *scope.machinery] if owners is None else scope.files)
            if not (owners or {}).get(path))
        if unowned:
            # Buck2 names no reader, so no selection narrower than the suite is exact.
            print("dev-test: no Buck2 target declares " + " ".join(unowned)
                  + "; selecting the whole suite", file=sys.stderr)
            broad = True
        else:
            seeds += sorted({label for labels in owners.values() for label in labels})
    if dependents and (seeds or broad):
        query = None if broad else reverse_dependencies(seeds)
        broad = query is None
        # Split the reachable tests so deferred and manual skips are reported
        # separately. The shared assembler applies the deferred policy.
        selected, deferred, skipped = ci_plan.dependent_test_labels(query, ROOT)
        if not broad:
            members = selected
    # CI and package-scoped iteration keep the touched packages' tail.
    # `--dependents` leaves it to the hourly main run unless opted back in.
    tail = sorted({*ci_plan.pr_tail_labels(paths, ROOT), *deferred})
    run_deferred = not dependents or include_deferred
    scope = ci_plan.DevTestScope(
        tuple(sorted(packages)), broad, facade, False, (),
        scope.targets, scope.files, scope.machinery)
    package_builds = ci_plan.package_build_labels(set(packages), ROOT)
    labels, builds = ci_plan.affected_buck2_labels(
        scope, members, tail, batches, package_builds,
        include_deferred=run_deferred,
    )
    if builds:
        commands.append(["kiln", "build", *builds])
    if labels:
        commands.append(["kiln", "test", *quick_test_args(paths), *labels])
    result = {
        "base": base,
        "head": git("rev-parse", "HEAD").decode().strip(),
        "inputs": identity,
        "identity_scope": "checkout/config snapshot; Buck2 validates full action inputs",
        "changed_files": paths,
        "selection": "suite" if broad else "dependents" if dependents else "packages",
        "commands": commands,
        "skipped": skipped,
        "skipped_deferred": [] if run_deferred else tail,
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


def failed_targets(report_path: Path) -> list[tuple[str, list[str]]]:
    """Each failed Buck2 label and its materialized result paths."""
    try:
        report = json.loads(report_path.read_text(encoding="utf-8"))
    except (OSError, ValueError):
        return []
    targets = []
    for label, result in sorted(report.get("results", {}).items()):
        if result.get("status") in {"PASS", "SUCCESS"}:
            continue
        outputs = result.get("outputs") or {}
        artifacts = [str(path) for path in (
            outputs.get("junit_xml"), outputs.get("log"), outputs.get("undeclared")
        ) if path]
        if result.get("stdout"):
            artifacts.append(str(result["stdout"]))
        if result.get("stderr"):
            artifacts.append(str(result["stderr"]))
        targets.append((label.removeprefix("root"), artifacts))
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


def failure_summary(command: list[str], code: int, log_path: Path, report_path: Path) -> str:
    """The ~40-line digest printed when a validation command fails."""
    try:
        text = log_path.read_text(errors="replace")
    except OSError:
        text = ""
    lines = [
        f"dev-test: `{shlex.join(command)}` failed with exit {code}",
        f"dev-test: full output: {log_path}",
    ]
    targets = failed_targets(report_path)
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


def test_statuses(report_path: Path) -> dict[str, dict]:
    """Each Buck2 label's result in one `kiln test` report."""
    try:
        results = json.loads(report_path.read_text(encoding="utf-8")).get("results", {})
    except (OSError, ValueError):
        return {}
    return {
        label.removeprefix("root"): result
        for label, result in results.items() if isinstance(result, dict)
    }


def gate_summary(planned: dict, ran: list[tuple[list[str], int, Path]], seconds: float) -> tuple[bool, list[str]]:
    """Whether every command passed, and the closing lines that say so.

    The lines carry the test target count and name every target that did not
    pass with its status (FAIL, TIMEOUT, INFRA_FAILURE). A command that fails
    without a failing target is a build or infrastructure error. Nothing is
    truncated: this is what a lane quotes as its gate result.
    """
    results: dict[str, dict] = {}
    for command, _code, report_path in ran:
        if command[:2] == ["kiln", "test"]:
            results.update(test_statuses(report_path))
    failed = sorted(
        (label, str(result.get("status"))) for label, result in results.items()
        if result.get("status") not in {"PASS", "SUCCESS"}
    )
    errors = [(command, code) for command, code, _report in ran if code]
    finished = [command for command, _code, _report in ran]
    not_run = [command for command in planned["commands"] if command not in finished]
    lines = []
    if failed:
        lines.append(f"dev-test: FAIL: {len(failed)} of {len(results)} test targets did not pass ({seconds:.0f}s)")
        lines += [f"  {status} {label}" for label, status in failed]
    for command, code in errors:
        if not failed or command[:2] != ["kiln", "test"]:
            lines.append(f"dev-test: ERROR: `{shlex.join(command[:3])}{' ...' if len(command) > 3 else ''}` exit {code} "
                         "without a failing test target (build, script or infrastructure error)")
    if not_run:
        lines.append("dev-test: NOT RUN: " + "; ".join(shlex.join(command[:3]) for command in not_run))
    passed = not failed and not errors and not not_run
    if passed:
        cached = sum(1 for result in results.values() if result.get("cache"))
        lines.append(f"dev-test: PASS: {len(results)} test targets ({cached} from cache), "
                     f"{len(ran)} commands, {seconds:.0f}s")
    if planned.get("skipped"):
        lines.append(f"dev-test: SKIPPED {len(planned['skipped'])} affected tests this gate never runs "
                     "(service-backed, Cargo-owned or trunk-only): " + " ".join(planned["skipped"]))
    if planned.get("skipped_deferred"):
        lines.append(f"dev-test: skipped {len(planned['skipped_deferred'])} dev-deferred targets "
                     "(run hourly on main): " + " ".join(planned["skipped_deferred"]))
    return passed, lines


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
        # Only Buck2 knows the full declared action inputs, including ignored
        # package data and external toolchains. A prior receipt cannot replace
        # its cache validation, even for an overlapping identical request.
        started = time.time_ns()
        save(directory / "plan.json", planned)
        print(f"dev-test: checkout/config snapshot {planned['inputs'][:12]}, {planned['selection']}", flush=True)
        # The repository half (script gates) and the Buck2 half (`kiln`
        # commands) share no input but the checkout, so they run side by
        # side, each in plan order. Every command keeps its own log and
        # digest, and one summary and one exit code cover both halves.
        halves = [
            [index for index, command in enumerate(planned["commands"]) if (command[0] == "kiln") == buck2]
            for buck2 in (True, False)
        ]
        halves = [half for half in halves if half]
        results: dict[int, tuple[list[str], int, Path]] = {}
        live: set[subprocess.Popen] = set()
        guard = threading.Lock()
        stop = threading.Event()
        faults: list[OSError] = []

        def run_half(indices: list[int], stream: bool) -> None:
            for index in indices:
                command = planned["commands"][index]
                log_path = directory / f"command-{index}.log"
                report_path = directory / f"test-report-{index}.json"
                output_dir = directory / f"test-results-{index}"
                executed = list(command)
                # A command that dies before reporting must not be read
                # through the previous run's report.
                report_path.unlink(missing_ok=True)
                if executed[:2] == ["kiln", "test"]:
                    executed[2:2] = [
                        "--test-report", str(report_path),
                        "--test-output-dir", str(output_dir),
                    ]
                try:
                    with guard:
                        if stop.is_set():
                            return
                        print("+ " + shlex.join(command), flush=True)
                        if stream:
                            process = subprocess.Popen(executed, cwd=ROOT, start_new_session=True)
                        else:
                            # Command output goes to a per-command log; a
                            # failure prints the digest of it, not the whole log.
                            with log_path.open("wb") as sink:
                                process = subprocess.Popen(
                                    executed, cwd=ROOT, start_new_session=True,
                                    stdout=sink, stderr=subprocess.STDOUT)
                        live.add(process)
                except OSError as error:
                    faults.append(error)
                    return
                status = process.wait()
                with guard:
                    live.discard(process)
                    if stop.is_set():
                        return
                    results[index] = (command, status, report_path)
                    # Keep going after a failure: one gate run names every
                    # failure, and a failed script proof does not hide the
                    # Rust verdicts.
                    if stream:
                        continue
                    if verbose:
                        # The half that did not own the terminal: its output
                        # is one block, not interleaved with the stream.
                        print(f"dev-test: output of `{shlex.join(command)}` (exit {status}):", flush=True)
                        sys.stdout.write(log_path.read_text(errors="replace"))
                        sys.stdout.flush()
                    elif status:
                        print(failure_summary(command, status, log_path, report_path), flush=True)
                    else:
                        print(f"dev-test: exit 0, output {log_path}", flush=True)

        code = 0
        # The first half owns this thread, so an interrupt lands in its wait.
        others = [
            threading.Thread(target=run_half, args=(half, False), daemon=True)
            for half in halves[1:]
        ]
        try:
            for thread in others:
                thread.start()
            if halves:
                run_half(halves[0], verbose)
            for thread in others:
                thread.join()
        except KeyboardInterrupt:
            with guard:
                stop.set()
                owned = list(live)
            for process in owned:
                if process.poll() is None:
                    os.killpg(process.pid, signal.SIGTERM)
            for process in owned:
                try:
                    process.wait(timeout=10)
                except subprocess.TimeoutExpired:
                    os.killpg(process.pid, signal.SIGKILL)
                    process.wait()
            for thread in others:
                thread.join(timeout=10)
            code = 130
        ran = [results[index] for index in sorted(results)]
        if faults:
            print(f"dev-test: {faults[0]}", file=sys.stderr)
            code = code or 2
        # The first failure in plan order is the exit code.
        code = code or next((status for _command, status, _report in ran if status), 0)
        passed, summary = gate_summary(planned, ran, (time.time_ns() - started) / 1e9)
        print("\n".join(summary), flush=True)
        if not passed and code == 0:
            # A report with a target that did not pass is never a green gate.
            code = 1
        unchanged = input_id(planned["base"]) == planned["inputs"]
        if not unchanged:
            print("dev-test: checkout/config snapshot changed during validation; result is stale", file=sys.stderr)
            code = 2
        receipt = {
            "plan": planned, "started_ns": started, "finished_ns": time.time_ns(),
            "exit_code": max(code, 0) if code >= 0 else 128 - code,
            "inputs_unchanged": unchanged,
            "summary": summary,
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
    parser.add_argument("--dependents", action="store_true", help="Include Buck2 reverse dependencies")
    parser.add_argument("--include-deferred", action="store_true", help="Include dev-deferred targets in the pre-land gate")
    parser.add_argument("--dry-run", action="store_true", help="Print the exact plan as JSON without executing")
    parser.add_argument("--verbose", action="store_true", help="Stream each command's output instead of logging it and summarizing failures")
    args = parser.parse_args()
    if any(os.environ.get(name) for name in LIVE_STORES):
        parser.error("live store URLs are not accepted; CI owns Postgres/S3/E2E")
    if not (ROOT / ".buckconfig.local").is_file():
        parser.error("run inside a Kiln fork")
    if args.base:
        base = git("rev-parse", "--verify", args.base + "^{commit}").decode().strip()
    else:
        base = git("merge-base", "HEAD", "origin/main").decode().strip()
    try:
        planned = plan(base, args.dependents, args.include_deferred)
    except (RuntimeError, KeyError, ValueError, OSError) as error:
        parser.error(str(error))
    if args.dry_run:
        print(json.dumps(planned, indent=2))
        return 0
    return run(planned, args.verbose)


if __name__ == "__main__":
    sys.exit(main())
