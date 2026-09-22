#!/usr/bin/env python3
"""Capture and compare isolated Bazel diagnostic bundles; execution stays in Kiln."""

import argparse
import hashlib
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time
import uuid

ROOT = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(ROOT / "tools" / "bazel"))
from diagnostics import BB_VERSION, bb_binary, summarize  # noqa: E402


def source_identity():
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


def save_manifest(bundle, manifest):
    temporary = bundle / "manifest.json.tmp"
    temporary.write_text(json.dumps(manifest, indent=2) + "\n")
    temporary.replace(bundle / "manifest.json")


def capture(args, binary):
    if args.output.resolve().is_relative_to(ROOT):
        raise ValueError("Keep diagnostic bundles outside the checkout")
    if args.baseline and not (args.baseline / "execution.zst").is_file():
        raise ValueError("Baseline must contain an existing execution.zst")
    if args.output.exists():
        raise ValueError("Output bundle already exists; choose a new directory")
    args.output.mkdir(parents=True, mode=0o700)
    bundle = args.output.resolve()
    invocation = str(uuid.uuid4())
    flags = args.bazel_args
    if flags and flags[0] == "--":
        flags = flags[1:]
    for flag in flags:
        if flag.split("=", 1)[0] in {
            "--profile",
            "--remote_grpc_log",
            "--experimental_remote_grpc_log",
            "--execution_log_compact_file",
            "--experimental_execution_log_compact_file",
            "--invocation_id",
        }:
            raise ValueError(f"Capture owns {flag.split('=', 1)[0]}")
    command = [
        "kiln",
        args.operation,
        f"--invocation_id={invocation}",
        f"--profile={bundle / 'profile.json.gz'}",
        "--noslim_profile",
        "--experimental_profile_include_target_label",
        f"--execution_log_compact_file={bundle / 'execution.zst'}",
        f"--remote_grpc_log={bundle / 'grpc.bin'}",
        *flags,
    ]
    manifest = {
        "version": 1,
        "invocation_id": invocation,
        "source": source_identity(),
        "cwd": str(ROOT),
        "command": command,
        "decoder": BB_VERSION,
        "bazel_version": (ROOT / ".bazelversion").read_text().strip(),
        "config_sha256": {
            name: hashlib.sha256((ROOT / name).read_bytes()).hexdigest()
            for name in (".bazelrc", ".kiln.bazelrc")
            if (ROOT / name).is_file()
        },
        "baseline": str(args.baseline.resolve()) if args.baseline else None,
        "state": "running",
        "started_unix": time.time(),
    }
    save_manifest(bundle, manifest)
    started = time.monotonic()
    interrupted = []
    child = None

    def forward(signum, _frame):
        interrupted.append(signum)
        if child is not None:
            try:
                os.killpg(child.pid, signum)
            except ProcessLookupError:
                pass

    handlers = {
        sig: signal.signal(sig, forward) for sig in (signal.SIGINT, signal.SIGTERM)
    }
    try:
        try:
            with (bundle / "build.log").open("w") as log:
                child = subprocess.Popen(
                    command,
                    cwd=ROOT,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    start_new_session=True,
                )
                print(f"Bundle: {bundle}\nBuild output: {bundle / 'build.log'}", flush=True)
                status = child.wait()
            manifest.update(
                build_exit_code=status,
                build_seconds=time.monotonic() - started,
                state="interrupted"
                if interrupted
                else ("succeeded" if status == 0 else "failed"),
            )
        except OSError as error:
            status = 127
            manifest.update(build_exit_code=status, state="failed", error=str(error))
        finally:
            manifest["finished_unix"] = time.time()
            try:
                manifest["source_after"] = source_identity()
                manifest["source_changed_during_build"] = (
                    manifest["source"] != manifest["source_after"]
                )
            except (OSError, subprocess.CalledProcessError) as error:
                manifest.update(
                    source_changed_during_build=None, source_identity_error=str(error)
                )
            if interrupted:
                manifest["state"] = "interrupted"
            save_manifest(bundle, manifest)
        decoded_at = time.monotonic()
        try:
            result = summarize(binary, bundle)
            manifest["diagnostics_complete"] = (
                result["complete"] and "source_identity_error" not in manifest
            )
            if args.baseline:
                compare(binary, args.baseline, bundle)
        except (OSError, ValueError, subprocess.CalledProcessError) as error:
            manifest.update(diagnostics_complete=False, diagnostics_error=str(error))
        manifest["analysis_seconds"] = time.monotonic() - decoded_at
        if interrupted:
            manifest.update(state="interrupted", diagnostics_complete=False)
        save_manifest(bundle, manifest)
        return (
            (128 + interrupted[-1])
            if interrupted
            else (status if status >= 0 else 128 - status)
        )
    finally:
        for sig, handler in handlers.items():
            signal.signal(sig, handler)


def compare(binary, baseline, bundle):
    for path in (baseline, bundle):
        if not (path / "execution.zst").is_file():
            raise ValueError(f"Missing compact execution log in {path}")
    with (bundle / "explain.txt").open("w") as output:
        subprocess.run(
            [
                str(binary),
                "explain",
                "--old",
                str(baseline.resolve() / "execution.zst"),
                "--new",
                str(bundle.resolve() / "execution.zst"),
            ],
            stdout=output,
            stderr=subprocess.STDOUT,
            check=True,
        )
    print(f"Input/flag/property comparison: {bundle / 'explain.txt'}")


def main():
    os.umask(0o077)
    parser = argparse.ArgumentParser(description=__doc__)
    sub = parser.add_subparsers(dest="mode", required=True)
    run = sub.add_parser(
        "capture", help="run Kiln and save a private, uniquely named bundle"
    )
    run.add_argument("--output", type=Path, required=True)
    run.add_argument("--baseline", type=Path)
    run.add_argument("operation", choices=("build", "check", "test", "clippy", "doc"))
    run.add_argument("bazel_args", nargs=argparse.REMAINDER)
    report = sub.add_parser(
        "report", help="decode an existing bundle without executing Bazel"
    )
    report.add_argument("bundle", type=Path)
    diff = sub.add_parser("compare", help="explain two explicit local bundles offline")
    diff.add_argument("baseline", type=Path)
    diff.add_argument("bundle", type=Path)
    args = parser.parse_args()
    binary = bb_binary()
    if args.mode == "capture":
        return capture(args, binary)
    if args.mode == "report":
        return 0 if summarize(binary, args.bundle)["complete"] else 1
    compare(binary, args.baseline, args.bundle)
    return 0


if __name__ == "__main__":
    try:
        sys.exit(main())
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        print(f"bazel-diagnose: {error}", file=sys.stderr)
        sys.exit(1)
