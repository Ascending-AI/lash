#!/usr/bin/env python3
"""Capture and compare isolated Buck2 diagnostic bundles through Kiln."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


ROOT = Path(__file__).resolve().parents[1]


def source_identity() -> dict[str, str]:
    paths = subprocess.check_output(
        ["git", "ls-files", "-z", "--cached", "--others", "--exclude-standard"],
        cwd=ROOT,
    ).split(b"\0")
    digest = hashlib.sha256()
    for name in sorted(set(paths) - {b""}):
        path = ROOT / os.fsdecode(name)
        digest.update(name + b"\0")
        if path.is_symlink():
            digest.update(b"symlink\0" + os.fsencode(os.readlink(path)))
        elif path.is_file():
            digest.update(str(path.stat().st_mode & 0o111).encode() + b"\0")
            with path.open("rb") as source:
                digest.update(hashlib.file_digest(source, "sha256").digest())
        else:
            digest.update(b"missing")
    return {
        "revision": subprocess.check_output(
            ["git", "rev-parse", "HEAD"], cwd=ROOT, text=True
        ).strip(),
        "content_sha256": digest.hexdigest(),
    }


def save_manifest(bundle: Path, manifest: dict) -> None:
    temporary = bundle / "manifest.json.tmp"
    temporary.write_text(json.dumps(manifest, indent=2) + "\n")
    temporary.replace(bundle / "manifest.json")


def buck2_client() -> str:
    result = subprocess.run(
        [sys.executable, "tools/buck2/bootstrap.py"],
        cwd=ROOT,
        capture_output=True,
        text=True,
        check=True,
    )
    return result.stdout.strip()


def analyze(bundle: Path) -> None:
    client = buck2_client()
    event_log = bundle / "events.json-lines"
    commands = {
        "summary.txt": [client, "log", "summary", str(event_log)],
        "critical-path.txt": [
            client,
            "log",
            "critical-path",
            "--format",
            "readable",
            str(event_log),
        ],
        "what-failed.json-lines": [
            client,
            "log",
            "what-failed",
            "--format",
            "json",
            str(event_log),
        ],
    }
    for filename, command in commands.items():
        with (bundle / filename).open("w") as output:
            subprocess.run(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, check=True)


def compare(baseline: Path, bundle: Path) -> None:
    client = buck2_client()
    command = [
        client,
        "log",
        "diff",
        "action-divergence",
        "--path1",
        str((baseline / "events.json-lines").resolve()),
        "--path2",
        str((bundle / "events.json-lines").resolve()),
    ]
    with (bundle / "action-divergence.txt").open("w") as output:
        subprocess.run(command, cwd=ROOT, stdout=output, stderr=subprocess.STDOUT, check=True)


def capture(args: argparse.Namespace) -> int:
    if args.output.resolve().is_relative_to(ROOT):
        raise ValueError("Keep diagnostic bundles outside the checkout")
    if args.baseline and not (args.baseline / "events.json-lines").is_file():
        raise ValueError("Baseline must contain events.json-lines")
    if args.output.exists():
        raise ValueError("Output bundle already exists; choose a new directory")
    args.output.mkdir(parents=True, mode=0o700)
    bundle = args.output.resolve()
    flags = args.buck2_args[1:] if args.buck2_args[:1] == ["--"] else args.buck2_args
    owned = {"--event-log", "--build-report", "--command-report-path"}
    for flag in flags:
        if flag.split("=", 1)[0] in owned:
            raise ValueError(f"Capture owns {flag.split('=', 1)[0]}")
    command = [
        "kiln",
        args.operation,
        "--event-log",
        str(bundle / "events.json-lines"),
        "--build-report",
        str(bundle / "build-report.json"),
        "--command-report-path",
        str(bundle / "command-report.json"),
        *flags,
    ]
    manifest = {
        "version": 2,
        "source": source_identity(),
        "cwd": str(ROOT),
        "command": command,
        "baseline": str(args.baseline.resolve()) if args.baseline else None,
        "state": "running",
        "started_unix": time.time(),
    }
    save_manifest(bundle, manifest)
    child: subprocess.Popen | None = None
    interrupted: list[int] = []

    def forward(signum: int, _frame: object) -> None:
        interrupted.append(signum)
        if child is not None:
            try:
                os.killpg(child.pid, signum)
            except ProcessLookupError:
                pass

    handlers = {
        sig: signal.signal(sig, forward) for sig in (signal.SIGINT, signal.SIGTERM)
    }
    started = time.monotonic()
    status = 127
    try:
        with (bundle / "build.log").open("w") as log:
            child = subprocess.Popen(
                command,
                cwd=ROOT,
                stdout=log,
                stderr=subprocess.STDOUT,
                start_new_session=True,
            )
            status = child.wait()
        manifest.update(
            build_exit_code=status,
            build_seconds=time.monotonic() - started,
            state="interrupted" if interrupted else "succeeded" if status == 0 else "failed",
        )
        try:
            manifest["source_after"] = source_identity()
            manifest["source_changed_during_build"] = manifest["source"] != manifest["source_after"]
        except (OSError, subprocess.CalledProcessError) as error:
            manifest.update(source_changed_during_build=None, source_identity_error=str(error))
        if (bundle / "events.json-lines").is_file():
            try:
                analyze(bundle)
                if args.baseline:
                    compare(args.baseline, bundle)
                manifest["diagnostics_complete"] = True
            except (OSError, subprocess.CalledProcessError, RuntimeError) as error:
                manifest.update(diagnostics_complete=False, diagnostics_error=str(error))
        else:
            manifest.update(diagnostics_complete=False, diagnostics_error="event log missing")
        if interrupted:
            manifest.update(state="interrupted", diagnostics_complete=False)
        manifest["finished_unix"] = time.time()
        save_manifest(bundle, manifest)
        if interrupted:
            return 128 + interrupted[-1]
        return 128 - status if status < 0 else status
    finally:
        for sig, handler in handlers.items():
            signal.signal(sig, handler)


def main() -> int:
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    run = sub.add_parser("capture")
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("--baseline", type=Path)
    run.add_argument("operation", choices=("build", "check", "test", "clippy", "doc"))
    run.add_argument("buck2_args", nargs=argparse.REMAINDER)
    report = sub.add_parser("report")
    report.add_argument("bundle", type=Path)
    diff = sub.add_parser("compare")
    diff.add_argument("baseline", type=Path)
    diff.add_argument("bundle", type=Path)
    args = parser.parse_args()
    try:
        if args.mode == "capture":
            return capture(args)
        if args.mode == "report":
            analyze(args.bundle)
            return 0
        compare(args.baseline, args.bundle)
        return 0
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        parser.error(str(error))


if __name__ == "__main__":
    raise SystemExit(main())
