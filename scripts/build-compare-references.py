#!/usr/bin/env python3
"""Build pinned optional comparison engines in an explicit cache, without vendoring.

Run through `kiln gate lash <fork> -- python3 scripts/build-compare-references.py
--cache target/compare-references`. The parent must apply the 8 GiB virtual cap.
"""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile

sys.path.insert(0, str(Path(__file__).resolve().parent))
from perf_land_ceilings import REFERENCE_ENVIRONMENT  # noqa: E402

PINS = {
    "quickjs": ("https://github.com/bellard/quickjs.git", "04be246001599f5995fa2f2d8c91a0f198d3f34c"),
    "dquickts": ("https://github.com/stencil-hq/dquickts.git", "f9a3efaa4d7cadb9dcad7e80a03d2774eb95a406"),
}


def run(*cmd, cwd=None, env=None):
    subprocess.run(cmd, cwd=cwd, env=env, check=True)


def fetch(root, url, revision):
    if not root.exists():
        run("git", "init", str(root))
        run("git", "remote", "add", "origin", url, cwd=root)
    have = subprocess.run(["git", "cat-file", "-e", revision + "^{commit}"], cwd=root, capture_output=True)
    if have.returncode:
        run("git", "fetch", "--depth=1", "origin", revision, cwd=root)


def build_quickjs(cache, url, revision):
    """Build the pinned revision from a pristine export in a fresh directory,
    under the reference environment, with the build directory mapped out of
    the binary, and install it under its own digest. The bytes depend on the
    revision and the compiler, not on where the cache lives. The upstream
    Makefile appends its flags to CFLAGS, so its optimization is unchanged."""
    mirror = cache / f"quickjs-{revision}"
    fetch(mirror, url, revision)
    with tempfile.TemporaryDirectory(dir=cache, prefix=".build-quickjs-") as build:
        source = subprocess.run(["git", "archive", revision], cwd=mirror, check=True, capture_output=True).stdout
        subprocess.run(["tar", "-x", "-C", build], input=source, check=True)
        env = dict(REFERENCE_ENVIRONMENT["env"], CFLAGS=f"-ffile-prefix-map={build}=quickjs")
        run("make", "-j2", "qjs", cwd=build, env=env)
        binary = Path(build) / "qjs"
        sha256 = hashlib.sha256(binary.read_bytes()).hexdigest()
        target = cache / f"qjs-{sha256}"
        binary.chmod(0o755)
        os.replace(binary, target)
    return {"repository": url, "revision": revision, "qjs": str(target), "sha256": sha256}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--cache", type=Path, required=True)
    parser.add_argument("--engine", action="append", choices=[*PINS, "monty"],
                        help="select references; default builds/fetches all")
    args = parser.parse_args()
    cache = args.cache.resolve()
    cache.mkdir(parents=True, exist_ok=True)
    receipts = {}
    for name in args.engine or [*PINS, "monty"]:
        if name == "monty":
            spec = importlib.util.spec_from_file_location(
                "embedded", Path(__file__).parent / "perf/kernel/embedded.py")
            embedded = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(embedded)
            binary = embedded.engine_tools(True, names=["monty"], tools=cache)["monty"]
            url, checksum, _, version = embedded.RELEASES["monty"]
            receipts[name] = {"url": url, "archive_sha256": checksum, "version": version,
                              "binary": binary,
                              "binary_sha256": hashlib.sha256(Path(binary).read_bytes()).hexdigest()}
            continue
        url, revision = PINS[name]
        if name == "quickjs":
            receipts[name] = build_quickjs(cache, url, revision)
            continue
        root = cache / f"{name}-{revision}"
        if not root.exists():
            run("git", "init", str(root))
            run("git", "remote", "add", "origin", url, cwd=root)
        head = subprocess.run(["git", "rev-parse", "HEAD"], cwd=root, capture_output=True, text=True)
        if head.returncode or head.stdout.strip() != revision:
            run("git", "fetch", "--depth=1", "origin", revision, cwd=root)
            run("git", "checkout", "--detach", revision, cwd=root)
        run("make", "-j2", "qjs", cwd=root)
        run("make", "-j2", "build/hibernate", cwd=root / "tests/snapshot")
        receipts[name] = {"repository": url, "revision": revision, "qjs": str(root / "qjs")}
        receipts[name]["hibernate"] = str(root / "tests/snapshot/build/hibernate")
        receipts[name]["script"] = str(root / "tests/snapshot/js/hibernate-sync.js")
    (cache / "engines.json").write_text(json.dumps(receipts, indent=2) + "\n")
    print(json.dumps(receipts, indent=2))


if __name__ == "__main__":
    main()
