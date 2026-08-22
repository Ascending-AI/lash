#!/usr/bin/env python3
"""Content-addressed reuse of `rustdoc --output-format json` artifacts.

The API-example-coverage gate and the rustdoc gate together spawn seven
rustdoc-JSON generations, several of which are byte-identical between runs and
between gates.  Regenerating them is the whole cost of running either gate on
an unchanged tree, so this module keys each artifact by everything that can
change it and hands back the previous file when nothing did.

Only *generation* is reused.  The gates' checking logic always runs against the
document this module names, so a cache hit can never turn a failing tree green:
it can only make the compiler's answer arrive faster.

The key covers the package, the exact rustdoc command line, `RUSTDOCFLAGS`, the
toolchain (`rustc -Vv`), `Cargo.lock`, and the tracked content of the package's
own crate directory plus every path dependency it transitively reaches.  The
last of those comes from `git ls-files -s`, whose output is the index's blob
hashes -- content, not timestamps -- so a checkout that restores identical
bytes still hits.

Because the key reads the *index* rather than the worktree, an uncommitted edit
is invisible to it.  That is a correctness hazard, not a performance one, so
the cache fails closed: any dirty or untracked path under a keyed crate
directory bypasses the cache entirely for that artifact -- generated directly,
and not written back.

Entries live under the user cache root -- `$XDG_CACHE_HOME/lash/rustdoc-json`,
or `~/.cache/lash/rustdoc-json` -- so one cache serves every worktree of the
repository and survives `cargo clean`.  Relocate it with
`LASH_RUSTDOC_CACHE_DIR`; switch the whole mechanism off with
`LASH_RUSTDOC_CACHE=off`.  Neither the hit path nor the miss path can fail a
gate on its own: an unusable cache directory, an unreadable entry, or a package
`cargo metadata` does not know simply means generating as before.
"""

from __future__ import annotations

import argparse
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
from typing import Callable, Sequence


#: Home for cached artifacts under `LASH_RUSTDOC_CACHE_DIR`'s default, relative
#: to the user cache root.  Deliberately outside any target directory: the point
#: is to survive `cargo clean`, worktree churn, and branch switches.
CACHE_SUBDIRECTORY = Path("lash") / "rustdoc-json"
#: Total bytes the cache may hold before the oldest entries are pruned.
CACHE_BUDGET_BYTES = 5 * 1024 * 1024 * 1024

_METADATA: dict[Path, dict] = {}
_TOOLCHAIN: dict[Path, str] = {}


def enabled() -> bool:
    """Whether the cache is switched on for this process."""
    return os.environ.get("LASH_RUSTDOC_CACHE", "").strip().lower() != "off"


def cache_dir() -> Path | None:
    """The directory holding cached artifacts, or `None` when unusable.

    An unset or empty `LASH_RUSTDOC_CACHE_DIR` falls back to the user cache
    root, which is the one location a repository script can name without
    encoding one developer's checkout layout.  A directory that cannot be
    created disables the cache rather than failing a gate.
    """
    if not enabled():
        return None
    configured = os.environ.get("LASH_RUSTDOC_CACHE_DIR", "").strip()
    if configured:
        directory = Path(configured)
    else:
        root = os.environ.get("XDG_CACHE_HOME", "").strip()
        directory = (Path(root) if root else Path.home() / ".cache") / CACHE_SUBDIRECTORY
    try:
        directory.mkdir(parents=True, exist_ok=True)
    except OSError:
        return None
    return directory


def _git(repo: Path, *arguments: str) -> str:
    completed = subprocess.run(
        ["git", *arguments],
        cwd=repo,
        check=True,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
    )
    return completed.stdout


def toolchain_stamp(repo: Path) -> str:
    """`rustc -Vv`, which names the compiler that shapes the JSON schema."""
    if repo not in _TOOLCHAIN:
        completed = subprocess.run(
            ["rustc", "-Vv"],
            cwd=repo,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        _TOOLCHAIN[repo] = completed.stdout
    return _TOOLCHAIN[repo]


def _metadata(repo: Path) -> dict:
    if repo not in _METADATA:
        completed = subprocess.run(
            ["cargo", "metadata", "--format-version", "1"],
            cwd=repo,
            check=True,
            text=True,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
        )
        _METADATA[repo] = json.loads(completed.stdout)
    return _METADATA[repo]


def _matches(entry: dict, package: str) -> bool:
    """Whether a metadata package answers to a cargo package spec.

    Specs are version-qualified where the graph holds two majors, exactly as
    the callers write them on `cargo rustdoc -p`.
    """
    name, _, version = package.partition("@")
    if entry["name"] != name:
        return False
    return not version or entry["version"] == version or entry["version"].startswith(f"{version}.")


def keyed_paths(repo: Path, package: str) -> list[str]:
    """Repository-relative paths whose tracked content shapes `package`'s JSON.

    Two contributions.  First, the package's own crate directory plus every
    path dependency it reaches in the resolved graph -- the sources rustdoc
    reads.  Registry dependencies are excluded because `Cargo.lock` already
    pins them and their sources are immutable for a version.  Second, every
    workspace manifest, because feature selection for a *registry* package is
    decided in this repository even when the lockfile does not move; without
    them a third-party document, which contributes no crate directory at all,
    would be keyed on the lockfile alone.

    This assumes a crate's sources, `include_str!` payloads included, live
    inside its own directory or inside a path dependency's -- true across this
    workspace, and the reason nothing outside `crates/` is keyed.
    """
    metadata = _metadata(repo)
    packages = {entry["id"]: entry for entry in metadata["packages"]}
    resolve = metadata.get("resolve") or {"nodes": []}
    edges = {node["id"]: [dep["pkg"] for dep in node.get("deps", [])] for node in resolve["nodes"]}
    roots = [
        entry["id"]
        for entry in metadata["packages"]
        if _matches(entry, package) and entry["id"] in edges
    ]
    if not roots:
        raise KeyError(f"package not present in cargo metadata: {package}")

    seen: set[str] = set()
    frontier = list(roots)
    while frontier:
        identifier = frontier.pop()
        if identifier in seen:
            continue
        seen.add(identifier)
        frontier.extend(edges.get(identifier, ()))

    directories: set[str] = set()
    for identifier in seen:
        entry = packages.get(identifier)
        if entry is not None:
            directories.add(_relative(repo, Path(entry["manifest_path"]).parent))
    directories.discard("")

    manifests: set[str] = {"Cargo.toml"}
    for identifier in metadata.get("workspace_members", []):
        entry = packages.get(identifier)
        if entry is not None:
            manifests.add(_relative(repo, Path(entry["manifest_path"])))
    manifests.discard("")
    # A manifest already inside a keyed directory would only make `git` report
    # its file twice; the directory covers it.
    covered = {f"{directory}/" for directory in directories}
    manifests = {
        manifest
        for manifest in manifests
        if not any(manifest.startswith(prefix) for prefix in covered)
    }
    return sorted(directories | manifests)


def _relative(repo: Path, path: Path) -> str:
    """`path` relative to the repository, or `""` when it lies outside it."""
    try:
        return path.resolve().relative_to(repo).as_posix()
    except ValueError:
        # A registry or `~/.cargo` checkout, whose identity `Cargo.lock`
        # already carries.
        return ""


def dirty_paths(repo: Path, paths: Sequence[str]) -> list[str]:
    """Uncommitted or untracked entries under `paths`, worktree order."""
    if not paths:
        return []
    output = _git(repo, "status", "--porcelain", "--", *paths)
    return [line for line in output.splitlines() if line.strip()]


def cache_key(
    repo: Path,
    package: str,
    crate_name: str,
    command: Sequence[str],
    paths: Sequence[str],
) -> str:
    """Hex digest over every input that can change the generated JSON."""
    digest = hashlib.sha256()

    def absorb(label: str, payload: bytes) -> None:
        digest.update(label.encode("utf-8"))
        digest.update(b"\0")
        digest.update(len(payload).to_bytes(8, "big"))
        digest.update(payload)

    absorb("version", b"1")
    absorb("package", package.encode("utf-8"))
    absorb("crate", crate_name.encode("utf-8"))
    absorb("command", "\0".join(command).encode("utf-8"))
    absorb("rustdocflags", os.environ.get("RUSTDOCFLAGS", "").encode("utf-8"))
    absorb("toolchain", toolchain_stamp(repo).encode("utf-8"))
    lock = repo / "Cargo.lock"
    absorb("lock", lock.read_bytes() if lock.exists() else b"")
    absorb("paths", "\0".join(paths).encode("utf-8"))
    sources = _git(repo, "ls-files", "-s", "--", *paths) if paths else ""
    absorb("sources", sources.encode("utf-8"))
    return digest.hexdigest()


def _entry_path(directory: Path, key: str) -> Path:
    return directory / f"{key}.json"


def _report(message: str) -> None:
    print(f"rustdoc-json-cache: {message}", file=sys.stderr, flush=True)


def prune(directory: Path, budget: int = CACHE_BUDGET_BYTES) -> int:
    """Drop least-recently-modified entries until the cache fits `budget`.

    Returns the number of entries removed.  Losing an entry costs one
    regeneration, so failures to unlink are ignored rather than raised.
    """
    entries: list[tuple[float, int, Path]] = []
    total = 0
    for path in directory.glob("*.json"):
        try:
            stat = path.stat()
        except OSError:
            continue
        entries.append((stat.st_mtime, stat.st_size, path))
        total += stat.st_size
    removed = 0
    for _, size, path in sorted(entries):
        if total <= budget:
            break
        try:
            path.unlink()
        except OSError:
            continue
        total -= size
        removed += 1
    return removed


def _store(directory: Path, key: str, source: Path) -> None:
    """Publish `source` into the cache atomically."""
    destination = _entry_path(directory, key)
    temporary = directory / f".{key}.{os.getpid()}.tmp"
    try:
        shutil.copyfile(source, temporary)
        os.replace(temporary, destination)
    except OSError:
        try:
            temporary.unlink()
        except OSError:
            pass
        return
    prune(directory)


def ensure(
    *,
    repo: Path,
    package: str,
    crate_name: str,
    command: Sequence[str],
    destination: Path,
    generate: Callable[[], None],
) -> Path:
    """The rustdoc JSON for `package`, generated only when nothing else will do.

    `generate` must run the rustdoc invocation `command` describes and leave its
    output at `destination`; it is called on every miss and every bypass.
    Returns the path to *read*, which on a hit is the cache entry itself.

    A hit deliberately does not copy over `destination`.  Cargo decides whether
    to re-run rustdoc from its own fingerprints, not from the output file, so
    overwriting `destination` with another configuration's document would leave
    a later genuine miss free to consider itself fresh and hand its consumer
    the wrong JSON.  Leaving cargo's output alone keeps it honest -- and makes a
    hit free rather than a copy of a large document.
    """
    repo = repo.resolve()
    destination = Path(destination)
    directory = cache_dir()
    if directory is None:
        generate()
        return destination

    try:
        paths = keyed_paths(repo, package)
        dirty = dirty_paths(repo, paths)
    except (OSError, KeyError, subprocess.CalledProcessError) as error:
        _report(f"{package}: keying unavailable ({error}); generating")
        generate()
        return destination

    if dirty:
        _report(
            f"{package}: bypass -- {len(dirty)} uncommitted path(s) under keyed "
            f"crate dirs (first: {dirty[0].strip()})"
        )
        generate()
        return destination

    try:
        key = cache_key(repo, package, crate_name, command, paths)
    except (OSError, subprocess.CalledProcessError) as error:
        _report(f"{package}: keying unavailable ({error}); generating")
        generate()
        return destination

    entry = _entry_path(directory, key)
    if entry.exists():
        try:
            os.utime(entry, None)
        except OSError:
            # Only the prune order suffers; the entry is still readable.
            pass
        _report(f"{package}: hit {key[:12]} -> {entry}")
        return entry

    _report(f"{package}: miss {key[:12]}; generating")
    generate()
    if destination.exists():
        _store(directory, key, destination)
    return destination


def run_command(command: Sequence[str], *, cwd: Path, env: dict[str, str] | None = None) -> None:
    """Run a rustdoc generation command, echoing its output only on failure."""
    completed = subprocess.run(
        list(command),
        cwd=cwd,
        env=env,
        check=False,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.STDOUT,
    )
    if completed.returncode:
        print(completed.stdout, file=sys.stderr, end="")
        completed.check_returncode()


def _parse(argv: Sequence[str]) -> tuple[dict, list[str]]:
    parser = argparse.ArgumentParser(
        description="Generate rustdoc JSON through the content-addressed cache."
    )
    parser.add_argument("--repo", default=None, help="repository root (default: this checkout)")
    parser.add_argument("--package", required=True, help="cargo package being documented")
    parser.add_argument("--crate-name", required=True, help="rustdoc's crate name for the JSON")
    parser.add_argument(
        "--destination", required=True, help="where the rustdoc command leaves its JSON"
    )
    if "--" not in argv:
        parser.error("the rustdoc command must follow `--`")
    split = list(argv).index("--")
    namespace = parser.parse_args(argv[:split])
    command = list(argv[split + 1 :])
    if not command:
        parser.error("the rustdoc command must follow `--`")
    return vars(namespace), command


def main(argv: Sequence[str]) -> int:
    options, command = _parse(argv)
    repo = Path(options["repo"] or Path(__file__).resolve().parents[1])
    environment = os.environ.copy()
    environment["RUSTC_BOOTSTRAP"] = "1"
    document = ensure(
        repo=repo,
        package=options["package"],
        crate_name=options["crate_name"],
        command=command,
        destination=Path(options["destination"]),
        generate=lambda: run_command(command, cwd=repo, env=environment),
    )
    # The one thing on stdout: the path the caller must read.
    print(document)
    return 0


if __name__ == "__main__":
    sys.exit(main(sys.argv[1:]))
