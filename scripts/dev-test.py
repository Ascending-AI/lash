#!/usr/bin/env python3
"""Plan and run one change-scoped Kiln validation for an exact input state."""

from __future__ import annotations

import argparse
import fcntl
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]
LIVE_STORES = (
    "LASH_POSTGRES_DATABASE_URL", "LASH_REQUIRE_POSTGRES", "LASH_S3_ENDPOINT",
    "LASH_REQUIRE_S3", "LASH_MINIO_ENDPOINT", "LASH_REQUIRE_MINIO",
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


def select(paths: list[str]) -> tuple[list[str], bool, bool]:
    packages: set[str] = set()
    broad = facade = False
    for name in paths:
        path = Path(name)
        if path.suffix == ".md" or name.startswith(("docs/", "LICENSE")) or name == ".gitignore":
            continue
        if name in ("Cargo.toml", "Cargo.lock"):
            broad = facade = True
        elif len(path.parts) >= 3 and path.parts[0] in ("crates", "examples", "runbooks"):
            package = "/".join(path.parts[:2])
            if not (ROOT / package / "BUILD.bazel").is_file():
                broad = True
            else:
                packages.add("//" + package)
                facade |= package == "crates/lash"
            if path.name in ("Cargo.toml", "BUILD.bazel"):
                broad = True
        else:
            broad = True
    return sorted(packages), broad, facade


def plan(base: str, dependents: bool) -> dict:
    identity = input_id(base)
    paths = changed_files(base)
    packages, broad, facade = select(paths)
    labels = ["//:dev_tests"] if broad else [p + ":all" for p in packages]
    if dependents and packages and not broad:
        expression = (
            'kind("test", rdeps(//..., set('
            + " ".join(p + ":all" for p in packages)
            + '))) - attr(tags, "manual", //...)'
        )
        result = subprocess.run(
            ["bazel", "query", expression], cwd=ROOT, capture_output=True, text=True
        )
        if result.returncode:
            # An incomplete query is never a narrowed validation result.
            print(result.stderr, file=sys.stderr)
            labels = ["//:dev_tests"]
            broad = True
        else:
            labels = sorted(set(result.stdout.split())) or ["//:dev_tests"]
    if facade:
        labels.append("//crates/lash:ui_fixtures")
    result = {
        "base": base,
        "head": git("rev-parse", "HEAD").decode().strip(),
        "inputs": identity,
        "changed_files": paths,
        "selection": "suite" if broad else "dependents" if dependents else "packages",
        "command": ["kiln", "test", *labels] if labels else [],
        "remaining": ["Required CI gates; this is focused local validation, not full CI"],
    }
    result["id"] = hashlib.sha256(json.dumps(result, sort_keys=True).encode()).hexdigest()
    if input_id(base) != identity:
        raise RuntimeError("inputs changed while selecting tests; rerun")
    return result


def save(path: Path, value: dict) -> None:
    temporary = path.with_suffix(f".{os.getpid()}.tmp")
    temporary.write_text(json.dumps(value, indent=2) + "\n")
    temporary.replace(path)


def run(planned: dict, requested: int) -> int:
    directory = Path(git("rev-parse", "--path-format=absolute", "--git-path", "lash-validation").decode().strip())
    directory.mkdir(mode=0o700, exist_ok=True)
    with (directory / "lock").open("a") as lock:
        try:
            fcntl.flock(lock, fcntl.LOCK_EX | fcntl.LOCK_NB)
        except BlockingIOError:
            print("dev-test: joining the running validation or waiting for this fork", flush=True)
            fcntl.flock(lock, fcntl.LOCK_EX)
        if input_id(planned["base"]) != planned["inputs"]:
            print("dev-test: inputs changed while planning/waiting; rerun", file=sys.stderr)
            return 2
        receipt_path = directory / "latest.json"
        if receipt_path.exists():
            receipt = json.loads(receipt_path.read_text())
            if (receipt["plan"]["id"] == planned["id"]
                    and receipt["finished_ns"] >= requested):
                print(f"dev-test: joined exit {receipt['exit_code']}; receipt {receipt_path}")
                return receipt["exit_code"]
        started = time.time_ns()
        save(directory / "plan.json", planned)
        print(f"dev-test: input {planned['inputs'][:12]}, {planned['selection']}", flush=True)
        print("+ " + " ".join(planned["command"]), flush=True)
        process = None
        try:
            if planned["command"]:
                process = subprocess.Popen(planned["command"], cwd=ROOT, start_new_session=True)
                code = process.wait()
            else:
                code = 0
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
            print("dev-test: inputs changed during validation; result is stale", file=sys.stderr)
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
    requested = time.time_ns()
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
    except RuntimeError as error:
        parser.error(str(error))
    if args.dry_run:
        print(json.dumps(planned, indent=2))
        return 0
    return run(planned, requested)


if __name__ == "__main__":
    sys.exit(main())
