#!/usr/bin/env python3
"""Materialize pinned Node, LLVM and PostgreSQL inputs into Buck2's ignored native cell."""

from __future__ import annotations

import argparse
import hashlib
import json
import os
import pathlib
import shutil
import subprocess
import tempfile
import urllib.request
import zipfile

import bootstrap_store


HERE = pathlib.Path(__file__).resolve().parent
ROOT = HERE.parents[1]
LOCK_PATH = HERE / "native-tools-lock.json"
DEFAULT_OUTPUT = ROOT / ".buck2/native"


def sha256(path: pathlib.Path) -> str:
    value = hashlib.sha256()
    with path.open("rb") as handle:
        for chunk in iter(lambda: handle.read(1024 * 1024), b""):
            value.update(chunk)
    return value.hexdigest()


def load_lock() -> dict:
    lock = json.loads(LOCK_PATH.read_text(encoding="utf-8"))
    if lock.get("schema") != 1 or lock.get("host") != "x86_64-unknown-linux-gnu":
        raise SystemExit("unsupported native-tools lock")
    return lock


def archives_current(output: pathlib.Path, lock: dict) -> bool:
    receipt = output / ".lash-native-tools.json"
    if not receipt.is_file():
        return False
    try:
        installed = json.loads(receipt.read_text(encoding="utf-8"))
    except (OSError, json.JSONDecodeError):
        return False
    installed_archives = {
        name: {key: value for key, value in tool.items() if key != "required"}
        for name, tool in installed.get("tools", {}).items()
    }
    wanted_archives = {
        name: {key: value for key, value in tool.items() if key != "required"}
        for name, tool in lock["tools"].items()
    }
    if installed.get("schema") != lock["schema"] or installed.get("host") != lock["host"] or installed_archives != wanted_archives:
        return False
    return all(
        (output / name / required).exists()
        for name, tool in lock["tools"].items()
        for required in tool["required"]
    )


def current(output: pathlib.Path, lock: dict) -> bool:
    receipt = output / ".lash-native-tools.json"
    return archives_current(output, lock) and json.loads(receipt.read_text(encoding="utf-8")) == lock and (output / "BUCK").is_file() and (
        output / "BUCK"
    ).read_text(encoding="utf-8") == buck_file()


def download(tool: dict, archive: pathlib.Path) -> None:
    with urllib.request.urlopen(tool["url"], timeout=120) as source, archive.open("wb") as sink:
        shutil.copyfileobj(source, sink)
    actual = sha256(archive)
    if actual != tool["sha256"]:
        raise SystemExit(f"archive checksum mismatch: expected {tool['sha256']}, got {actual}")


def safe_members(archive: pathlib.Path, zstd: bool) -> list[str]:
    command = ["tar"]
    if zstd:
        command.append("--use-compress-program=unzstd")
    command.extend(["-tf", str(archive)])
    members = subprocess.run(command, check=True, capture_output=True, text=True).stdout.splitlines()
    if any(pathlib.PurePosixPath(item).is_absolute() or ".." in pathlib.PurePosixPath(item).parts for item in members):
        raise SystemExit(f"unsafe archive member in {archive}")
    return members


def inner_archive(tool: dict, archive: pathlib.Path) -> pathlib.Path:
    """The tar archive a jar or a Debian package wraps, named by `inner`."""
    inner = tool.get("inner")
    if inner is None:
        return archive
    if pathlib.PurePosixPath(inner).name != inner:
        raise SystemExit(f"unsafe inner archive name: {inner}")
    unpacked = archive.with_name(inner)
    if archive.suffix == ".deb":
        # A Debian package is an `ar` archive: an 8-byte magic, then a 60-byte
        # header and the even-padded bytes of each member.
        with archive.open("rb") as source:
            if source.read(8) != b"!<arch>\n":
                raise SystemExit(f"not a Debian package: {archive}")
            while header := source.read(60):
                size = int(header[48:58])
                if header[:16].decode("ascii").strip().rstrip("/") == inner:
                    unpacked.write_bytes(source.read(size))
                    return unpacked
                source.seek(size + size % 2, os.SEEK_CUR)
    else:
        with zipfile.ZipFile(archive) as outer:
            if inner in outer.namelist():
                unpacked.write_bytes(outer.read(inner))
                return unpacked
    raise SystemExit(f"{archive.name} has no member {inner}")


def extract(tool: dict, archive: pathlib.Path, destination: pathlib.Path, zstd: bool) -> None:
    members = safe_members(archive, zstd)
    prefix = tool.get("archive_prefix")
    if prefix is None:
        tops = {pathlib.PurePosixPath(item).parts[0] for item in members if pathlib.PurePosixPath(item).parts}
        prefix = next(iter(tops)) if len(tops) == 1 else ""
    command = ["tar"]
    if zstd:
        command.append("--use-compress-program=unzstd")
    command.extend(["-xf", str(archive), "-C", str(destination)])
    if prefix:
        command.extend(["--strip-components", str(len(prefix.split("/")))])
    command.extend(f"--exclude={pattern}" for pattern in tool.get("exclude", []))
    command.extend(tool.get("members", []))
    subprocess.run(command, check=True)
    resolve_links(destination)


def resolve_links(tree: pathlib.Path) -> None:
    """Replace each symlink to a file of the tree with that file.

    A versioned shared library ships as `lib.so.N -> lib.so.N.M`, and the
    loader asks for the link's name. The tree is an action input, so it holds
    plain files only: the last link to a file takes its place, an earlier one a
    copy.
    """
    links: dict[pathlib.Path, list[pathlib.Path]] = {}
    for link in sorted(path for path in tree.rglob("*") if path.is_symlink()):
        target = link.resolve()
        if not target.is_file() or tree.resolve() not in target.parents:
            raise SystemExit(f"unsupported link in a native archive: {link}")
        links.setdefault(target, []).append(link)
    for target, names in links.items():
        for link in names[:-1]:
            link.unlink()
            shutil.copy2(target, link)
        names[-1].unlink()
        target.rename(names[-1])


def buck_file() -> str:
    return '''# @generated by tools/buck2/bootstrap_native_tools.py; do not edit.
load("@root//tools/buck2:native_tree.bzl", "native_tree")

export_file(name = "node", src = "node/bin/node", mode = "reference", visibility = ["PUBLIC"])
native_tree(
    name = "llvm_tree",
    srcs = {p.removeprefix("llvm/"): p for p in glob(["llvm/**"])},
    visibility = ["PUBLIC"],
)
native_tree(
    name = "glibc_headers",
    srcs = {p.removeprefix("glibc_headers/"): p for p in glob(["glibc_headers/**"])},
    visibility = ["PUBLIC"],
)
native_tree(
    name = "kernel_headers",
    srcs = {p.removeprefix("kernel_headers/"): p for p in glob(["kernel_headers/**"])},
    visibility = ["PUBLIC"],
)
native_tree(
    name = "postgres",
    srcs = {p.removeprefix("postgres/"): p for p in glob(["postgres/**"])},
    visibility = ["PUBLIC"],
)
export_file(name = "nss_wrapper", src = "nss_wrapper/libnss_wrapper.so", mode = "reference", visibility = ["PUBLIC"])
'''


def build(name: str, tool: dict, destination: pathlib.Path) -> None:
    destination.mkdir()
    with tempfile.TemporaryDirectory(prefix="lash-native-archive-", dir=destination.parent) as raw:
        archive = pathlib.Path(raw) / pathlib.PurePosixPath(tool["url"]).name
        print(f"download {name} {tool['url']}")
        download(tool, archive)
        archive = inner_archive(tool, archive)
        extract(tool, archive, destination, archive.name.endswith(".zst"))


def install(output: pathlib.Path, lock: dict) -> None:
    output.parent.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix="lash-native-tools-", dir=output.parent) as raw:
        stage = pathlib.Path(raw) / "native"
        stage.mkdir()
        for name, tool in lock["tools"].items():
            # `required` only names paths to check; it does not change the tree.
            archive = {key: value for key, value in tool.items() if key != "required"}
            destination = stage / name
            if not bootstrap_store.materialize(
                ROOT, "native-" + name, archive, lambda tree, name=name, tool=tool: build(name, tool, tree), destination
            ):
                build(name, tool, destination)
        (stage / "BUCK").write_text(buck_file(), encoding="utf-8")
        (stage / ".lash-native-tools.json").write_text(
            json.dumps(lock, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
        missing = [
            f"{name}/{path}"
            for name, tool in lock["tools"].items()
            for path in tool["required"]
            if not (stage / name / path).exists()
        ]
        if missing:
            raise SystemExit("native archives omitted: " + ", ".join(missing))
        old = output.with_name(output.name + ".old")
        if old.exists():
            shutil.rmtree(old)
        if output.exists():
            output.rename(old)
        os.replace(stage, output)
        if old.exists():
            shutil.rmtree(old)


def main() -> int:
    parser = argparse.ArgumentParser()
    parser.add_argument("--check", action="store_true")
    parser.add_argument("--output", type=pathlib.Path, default=DEFAULT_OUTPUT)
    args = parser.parse_args()
    lock = load_lock()
    output = args.output.resolve()
    if current(output, lock):
        print(f"Buck2 native tools are current: {output}")
        return 0
    if args.check:
        print(f"Buck2 native tools are missing or stale: {output}")
        return 1
    updated_graph = archives_current(output, lock)
    if updated_graph:
        (output / "BUCK").write_text(buck_file(), encoding="utf-8")
        (output / ".lash-native-tools.json").write_text(
            json.dumps(lock, indent=2, sort_keys=True) + "\n", encoding="utf-8"
        )
    else:
        install(output, lock)
    if not current(output, lock):
        raise SystemExit("installed native tools failed their receipt check")
    print("updated graph for" if updated_graph else "installed", f"Buck2 native tools: {output}")
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
