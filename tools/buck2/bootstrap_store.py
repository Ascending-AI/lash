#!/usr/bin/env python3
"""Share pinned bootstrap inputs between checkouts through one verified store.

An entry is an immutable tree named by the pin it was built from and sealed by
a manifest of its files. It is built once, in a private directory, and renamed
into place only after its builder verified the pinned checksums.

A checkout never points Buck2 at the store: Buck2 does not read a source tree
through a symlink that leaves the project. Each input tree is therefore cloned
into the checkout as ordinary files: a reflink where the filesystem has them,
otherwise a hardlink, otherwise a copy. Hardlinks stay bounded, one per live
checkout and never more than LINK_CAP to an inode. Only a directory that Buck2
ignores (`vendor`) is a symlink into the store.

`LASH_BUCK2_STORE` names the store directory, or `off`. It defaults to
`$XDG_CACHE_HOME/lash-buck2`, and to `off` under CI. Callers keep their
per-checkout installation for a store that is off or unusable.
"""

from __future__ import annotations

from concurrent.futures import ThreadPoolExecutor
from contextlib import contextmanager
import errno
import fcntl
import hashlib
import json
import os
import pathlib
import shutil
import stat
import tempfile
import time


LAYOUT = "v1"
LINK_CAP = 1000
FICLONE = 0x40049409
DIRECTORIES = ("entries", "locks", "refs", "tmp", "sync-receipts")


class StoreError(RuntimeError):
    pass


def location() -> pathlib.Path | None:
    configured = os.environ.get("LASH_BUCK2_STORE")
    if configured is None:
        if os.environ.get("CI"):
            return None
        cache = os.environ.get("XDG_CACHE_HOME") or os.path.join(os.path.expanduser("~"), ".cache")
        configured = os.path.join(cache, "lash-buck2")
    if configured in ("", "off"):
        return None
    return pathlib.Path(configured).absolute() / LAYOUT


def usable() -> pathlib.Path | None:
    """Return the store root, or None when this user cannot own and write it."""
    root = location()
    if root is None:
        return None
    try:
        root.mkdir(mode=0o700, parents=True, exist_ok=True)
        for name in DIRECTORIES:
            (root / name).mkdir(mode=0o700, exist_ok=True)
        root = root.resolve()
        status = root.stat()
        if status.st_uid != os.getuid() or status.st_mode & 0o022:
            return None
        if not all(os.access(root / name, os.W_OK | os.X_OK) for name in DIRECTORIES):
            return None
    except OSError:
        return None
    return root


def receipts() -> pathlib.Path | None:
    root = usable()
    return root / "sync-receipts" if root else None


def entry_name(slot: str, identity) -> str:
    encoded = json.dumps(identity, sort_keys=True, separators=(",", ":")).encode()
    return f"{slot}-{hashlib.sha256(encoded).hexdigest()}"


def tree(slot: str, identity) -> pathlib.Path | None:
    """Where the store keeps this entry's tree, whether or not it exists yet."""
    root = usable()
    return root / "entries" / entry_name(slot, identity) / "tree" if root else None


@contextmanager
def _lock(root: pathlib.Path, name: str, exclusive: bool, blocking: bool = True):
    descriptor = os.open(root / "locks" / (name + ".lock"), os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        operation = fcntl.LOCK_EX if exclusive else fcntl.LOCK_SH
        try:
            fcntl.flock(descriptor, operation if blocking else operation | fcntl.LOCK_NB)
        except BlockingIOError:
            yield False
            return
        yield True
    finally:
        os.close(descriptor)


def _write(path: pathlib.Path, text: str) -> None:
    descriptor, temporary = tempfile.mkstemp(prefix=path.name + ".", dir=path.parent)
    with os.fdopen(descriptor, "w", encoding="utf-8") as output:
        output.write(text)
    os.replace(temporary, path)


def _remove(path: pathlib.Path) -> None:
    """Delete a tree whose directories may be sealed read-only."""
    if path.is_symlink() or not path.is_dir():
        path.unlink(missing_ok=True)
        return
    for directory, _, _ in os.walk(path):
        os.chmod(directory, 0o700)
    shutil.rmtree(path, ignore_errors=True)


def _sha256(path: str) -> str:
    with open(path, "rb") as source:
        return hashlib.file_digest(source, "sha256").hexdigest()


def _scan(top: pathlib.Path):
    """List a tree as sorted directories, {file: (size, mtime, mode, links)} and {symlink: target}."""
    directories, files, links = [], {}, {}
    pending = [""]
    while pending:
        parent = pending.pop()
        with os.scandir(os.path.join(top, parent)) as children:
            for child in children:
                name = f"{parent}/{child.name}" if parent else child.name
                if child.is_symlink():
                    links[name] = os.readlink(child.path)
                elif child.is_dir(follow_symlinks=False):
                    directories.append(name)
                    pending.append(name)
                elif child.is_file(follow_symlinks=False):
                    status = child.stat(follow_symlinks=False)
                    files[name] = (status.st_size, status.st_mtime_ns, stat.S_IMODE(status.st_mode), status.st_nlink)
                else:
                    raise StoreError(f"unsupported file type in a store tree: {child.path}")
    return sorted(directories), files, links


def _digests(top: pathlib.Path, names) -> dict[str, str]:
    names = list(names)
    with ThreadPoolExecutor(max_workers=8) as pool:
        return dict(zip(names, pool.map(lambda name: _sha256(os.path.join(top, name)), names)))


def _seal(top: pathlib.Path) -> dict:
    """Make a built tree read-only and describe every file in it."""
    directories, files, links = _scan(top)
    for name, (_, _, mode, _) in files.items():
        os.chmod(os.path.join(top, name), mode & ~0o222)
    digests = _digests(top, files)
    _, files, _ = _scan(top)
    for name in ["", *directories]:
        os.chmod(os.path.join(top, name), 0o555)
    return {
        "directories": directories,
        "files": {name: [size, mtime, mode, digests[name]] for name, (size, mtime, mode, _) in sorted(files.items())},
        "symlinks": links,
    }


def _inspect(entry: pathlib.Path, deep: bool = False):
    """Return (manifest, hardlink counts) for an entry that still matches its seal, else None."""
    try:
        manifest = json.loads((entry / "manifest.json").read_text(encoding="utf-8"))
        if manifest.get("schema") != 1 or manifest.get("entry") != entry.name:
            return None
        directories, files, links = _scan(entry / "tree")
        sealed = manifest["files"]
        if directories != manifest["directories"] or links != manifest["symlinks"] or files.keys() != sealed.keys():
            return None
        if any(list(files[name][:3]) != sealed[name][:3] for name in files):
            return None
        if deep and any(digest != sealed[name][3] for name, digest in _digests(entry / "tree", files).items()):
            return None
    except (OSError, ValueError, KeyError, TypeError, AttributeError, IndexError, StoreError):
        return None
    return manifest, {name: status[3] for name, status in files.items()}


def _clear_staging(root: pathlib.Path, name: str) -> None:
    for stale in (root / "tmp").glob(name + ".*"):
        _remove(stale)


def _discard(root: pathlib.Path, entry: pathlib.Path) -> None:
    dead = pathlib.Path(tempfile.mkdtemp(prefix=entry.name + ".", dir=root / "tmp")) / "dead"
    os.rename(entry, dead)
    _remove(dead.parent)


def _fill(root: pathlib.Path, name: str, identity, build) -> None:
    """Build an entry privately and publish it by one rename. Caller holds its exclusive lock."""
    _clear_staging(root, name)
    work = pathlib.Path(tempfile.mkdtemp(prefix=name + ".", dir=root / "tmp"))
    try:
        stage = work / "entry"
        stage.mkdir()
        build(stage / "tree")
        if not (stage / "tree").is_dir():
            raise StoreError(f"builder produced no tree for {name}")
        manifest = {"schema": 1, "entry": name, "identity": identity, **_seal(stage / "tree")}
        _write(stage / "manifest.json", json.dumps(manifest, sort_keys=True) + "\n")
        entry = root / "entries" / name
        if entry.exists() or entry.is_symlink():
            _discard(root, entry)
        os.rename(stage, entry)
    finally:
        _remove(work)


def _reflink(source: str, target: str) -> None:
    with open(source, "rb") as origin, open(target, "wb") as copy:
        fcntl.ioctl(copy.fileno(), FICLONE, origin.fileno())


class _Placer:
    """Place one stored file in a checkout by the cheapest mechanism that works."""

    def __init__(self) -> None:
        self.reflink = True
        self.hardlink = True

    def place(self, source: str, target: str, mode: int, links: int) -> None:
        if self.reflink:
            try:
                _reflink(source, target)
                os.chmod(target, mode)
                return
            except OSError:
                self.reflink = False
                if os.path.lexists(target):
                    os.unlink(target)
        if self.hardlink and links < LINK_CAP:
            try:
                os.link(source, target)
                return
            except OSError as error:
                # One inode at the filesystem's limit says nothing about the rest.
                if error.errno != errno.EMLINK:
                    self.hardlink = False
        shutil.copyfile(source, target)
        os.chmod(target, mode)


def _clone(top: pathlib.Path, manifest: dict, links: dict[str, int], destination: pathlib.Path) -> None:
    os.mkdir(destination)
    for name in manifest["directories"]:
        os.mkdir(os.path.join(destination, name))
    placer = _Placer()

    def place(item) -> None:
        name, (_, _, mode, _) = item
        placer.place(os.path.join(top, name), os.path.join(destination, name), mode, links[name])

    files = list(manifest["files"].items())
    for item in files[:1]:
        place(item)
    if placer.reflink or placer.hardlink:
        for item in files[1:]:
            place(item)
    else:
        with ThreadPoolExecutor(max_workers=8) as pool:
            list(pool.map(place, files[1:]))
    for name, target in manifest["symlinks"].items():
        os.symlink(target, os.path.join(destination, name))


def release(destination: pathlib.Path) -> None:
    """Remove a checkout's link into the store, leaving the store untouched."""
    if destination.is_symlink():
        destination.unlink()


def _point(top: pathlib.Path, destination: pathlib.Path) -> None:
    temporary = destination.with_name(destination.name + ".link")
    release(temporary)
    os.symlink(top, temporary)
    old = None
    if destination.is_dir() and not destination.is_symlink():
        old = destination.with_name(destination.name + ".old")
        _remove(old)
        destination.rename(old)
    os.replace(temporary, destination)
    if old is not None:
        _remove(old)


def _reference_path(root: pathlib.Path, checkout: pathlib.Path) -> tuple[str, pathlib.Path]:
    key = hashlib.sha256(str(checkout).encode()).hexdigest()
    return "ref-" + key, root / "refs" / (key + ".json")


def _reference(root: pathlib.Path, checkout: pathlib.Path, slot: str, name: str) -> None:
    """Record that this checkout uses this entry, so pruning keeps it."""
    lock, path = _reference_path(root, checkout)
    with _lock(root, lock, exclusive=True):
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
            if record.get("checkout") != str(checkout) or not isinstance(record.get("entries"), dict):
                raise ValueError
        except (OSError, ValueError, AttributeError):
            record = {"checkout": str(checkout), "entries": {}}
        if record["entries"].get(slot) != name:
            record["entries"][slot] = name
            _write(path, json.dumps(record, indent=2, sort_keys=True) + "\n")


def _referenced(root: pathlib.Path) -> set[str]:
    """Entries used by a checkout that still exists; forget the checkouts that do not."""
    live: set[str] = set()
    for path in (root / "refs").glob("*.json"):
        try:
            record = json.loads(path.read_text(encoding="utf-8"))
            checkout, entries = record["checkout"], set(record["entries"].values())
        except (OSError, ValueError, KeyError, TypeError, AttributeError):
            path.unlink(missing_ok=True)
            continue
        if os.path.isdir(checkout):
            live |= entries
        else:
            path.unlink(missing_ok=True)
    return live


def materialize(checkout: pathlib.Path, slot: str, identity, build, destination: pathlib.Path, symlink: bool = False) -> bool:
    """Give `destination` the tree pinned by `identity`, building it at most once for all checkouts.

    `build(tree)` must create the directory `tree`; its parent is private
    scratch space. `destination` must not exist, unless `symlink` replaces it.
    Returns False, having done nothing, when there is no usable store.
    """
    root = usable()
    if root is None:
        return False
    name = entry_name(slot, identity)
    entry = root / "entries" / name
    for _ in range(3):
        with _lock(root, name, exclusive=False):
            intact = _inspect(entry)
            if intact is not None:
                manifest, links = intact
                os.utime(entry / "manifest.json")
                _reference(root, checkout, slot, name)
                if symlink:
                    _point(entry / "tree", destination)
                else:
                    _clone(entry / "tree", manifest, links, destination)
                return True
        with _lock(root, name, exclusive=True):
            if _inspect(entry) is None:
                _fill(root, name, identity, build)
    raise StoreError(f"store entry {name} does not stay intact: {entry}")


def prune(unused_for: float) -> int:
    """Delete entries no existing checkout references and nothing used recently."""
    root = usable()
    if root is None:
        print("Buck2 bootstrap store is off or unusable; nothing to prune")
        return 0
    removed = kept = busy = freed = 0
    now = time.time()
    names = {path.name for path in (root / "entries").iterdir()}
    names |= {path.name.rsplit(".", 1)[0] for path in (root / "tmp").iterdir()}
    for name in sorted(names):
        entry = root / "entries" / name
        with _lock(root, name, exclusive=True, blocking=False) as held:
            if not held:
                busy += 1
                continue
            _clear_staging(root, name)
            if not entry.exists():
                continue
            try:
                manifest = json.loads((entry / "manifest.json").read_text(encoding="utf-8"))
                size = sum(record[0] for record in manifest["files"].values())
                used = (entry / "manifest.json").stat().st_mtime
            except (OSError, ValueError, KeyError, TypeError, AttributeError, IndexError):
                size, used = 0, None
            # References are read under the entry's lock: a checkout records
            # its reference under the same lock before it clones or links.
            if used is not None and (now - used < unused_for or name in _referenced(root)):
                kept += 1
                continue
            _discard(root, entry)
            removed += 1
            freed += size
    stale = 0
    for receipt in (root / "sync-receipts").iterdir():
        if now - receipt.stat().st_mtime >= unused_for:
            receipt.unlink(missing_ok=True)
            stale += 1
    print(
        f"pruned {removed} Buck2 bootstrap store entries ({freed // (1024 * 1024)} MiB) and {stale} sync receipts; "
        f"kept {kept}, skipped {busy} in use: {root}"
    )
    return 0


def verify() -> int:
    """Rehash every entry and delete each one that no longer matches its seal."""
    root = usable()
    if root is None:
        print("Buck2 bootstrap store is off or unusable; nothing to verify")
        return 0
    corrupt = []
    entries = sorted(path.name for path in (root / "entries").iterdir())
    for name in entries:
        entry = root / "entries" / name
        with _lock(root, name, exclusive=True):
            if entry.exists() and _inspect(entry, deep=True) is None:
                _discard(root, entry)
                corrupt.append(name)
    for name in corrupt:
        print(f"removed corrupt Buck2 bootstrap store entry: {name}")
    print(f"verified {len(entries) - len(corrupt)} of {len(entries)} Buck2 bootstrap store entries: {root}")
    return 1 if corrupt else 0
