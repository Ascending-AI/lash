#!/usr/bin/env python3
"""Install the pinned official executable into this checkout's ignored state.

Pinned inputs come from the shared store in bootstrap_store.py when it is
usable, and are otherwise downloaded into this checkout alone.
"""
import argparse
from contextlib import contextmanager
import fcntl
import hashlib
import json
import os
from pathlib import Path
import shutil
import stat
import subprocess
import sys
import tempfile
import urllib.request

import bootstrap_store

ROOT = Path(__file__).resolve().parents[2]
PIN = json.loads(Path(__file__).with_name('pins.json').read_text())['buck2']


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


@contextmanager
def preparation_lock(state):
    if state.is_symlink():
        raise SystemExit('Refusing a symlinked .buck2 state directory')
    state.mkdir(exist_ok=True)
    descriptor = os.open(state / 'prepare.lock', os.O_CREAT | os.O_RDWR | os.O_NOFOLLOW, 0o600)
    try:
        if not stat.S_ISREG(os.fstat(descriptor).st_mode):
            raise SystemExit('Buck2 preparation lock must be a regular file')
        fcntl.flock(descriptor, fcntl.LOCK_EX)
        yield descriptor
    finally:
        os.close(descriptor)


def prepare_prelude(executable, lock_fd):
    overlay = ROOT / 'tools/buck2/prelude_overlay.py'
    if not overlay.is_file():
        return
    prelude = ROOT / '.buck2/prelude'

    def apply():
        subprocess.run([sys.executable, str(overlay), '--buck2', str(executable), '--prelude-dir', str(prelude)], check=True, cwd=ROOT, stdout=subprocess.DEVNULL, pass_fds=(lock_fd,))

    def build(tree):
        # Buck2 expands its bundled prelude only at the cell's own path.
        apply()
        (prelude / '.lash-overlay.json').unlink()
        shutil.move(prelude, tree)

    if not prelude.is_dir():
        identity = {'buck2': PIN['executable_sha256'], 'overlay': digest(overlay)}
        bootstrap_store.materialize(ROOT, 'prelude', identity, build, prelude)
    # Verifies the patched files and writes this checkout's own receipt.
    apply()


def prepare_tools(executable, lock_fd):
    prepare_prelude(executable, lock_fd)
    for name in (
        'bootstrap_rust_toolchain.py',
        'bootstrap_native_tools.py',
        'bootstrap_reindeer.py',
        'bootstrap_vendor.py',
    ):
        script = ROOT / 'tools/buck2' / name
        if script.is_file():
            subprocess.run([sys.executable, str(script)], check=True, cwd=ROOT, stdout=subprocess.DEVNULL, pass_fds=(lock_fd,))


def prepare_client(args, state, lock_fd):
    destination = state / 'bin'
    if destination.is_symlink():
        raise SystemExit('Refusing a symlinked executable directory')
    destination.mkdir(exist_ok=True)
    executable = destination / 'buck2'
    if executable.is_file() and not executable.is_symlink() and digest(executable) == PIN['executable_sha256']:
        if not args.client_only:
            prepare_tools(executable, lock_fd)
        print(executable)
        return
    def fetch(tree):
        tree.mkdir()
        archive = tree.parent / 'buck2.zst'
        if args.archive:
            shutil.copyfile(args.archive, archive)
        else:
            with urllib.request.urlopen(PIN['url'], timeout=60) as source, archive.open('wb') as output:
                shutil.copyfileobj(source, output)
        if digest(archive) != PIN['archive_sha256']:
            raise SystemExit('Buck2 release archive checksum mismatch')
        binary = tree / 'buck2'
        subprocess.run(['zstd', '-q', '-d', str(archive), '-o', str(binary)], check=True, pass_fds=(lock_fd,))
        archive.unlink()
        if digest(binary) != PIN['executable_sha256']:
            raise SystemExit('Buck2 executable checksum mismatch')
        binary.chmod(0o755)

    with tempfile.TemporaryDirectory(prefix='bootstrap-', dir=state) as work:
        tree = Path(work) / 'client'
        identity = {key: PIN[key] for key in ('archive_sha256', 'executable_sha256')}
        if not bootstrap_store.materialize(ROOT, 'buck2', identity, fetch, tree):
            fetch(tree)
        binary = tree / 'buck2'
        if digest(binary) != PIN['executable_sha256']:
            raise SystemExit('Buck2 executable checksum mismatch')
        os.replace(binary, executable)
    if not args.client_only:
        prepare_tools(executable, lock_fd)
    print(executable)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', type=Path, help='Use an already downloaded, checksum-verified release archive.')
    parser.add_argument('--client-only', action='store_true', help='Prepare only the official client, for checkout cleanup.')
    parser.add_argument('--prune-store', action='store_true', help='Delete shared store entries that no existing checkout references and nothing used recently, then exit.')
    parser.add_argument('--unused-for', type=float, default=86400, metavar='SECONDS', help='With --prune-store, also keep an entry used this recently (default: one day).')
    parser.add_argument('--verify-store', action='store_true', help='Rehash every shared store entry, delete the corrupt ones, then exit.')
    args = parser.parse_args()
    if args.prune_store:
        raise SystemExit(bootstrap_store.prune(args.unused_for))
    if args.verify_store:
        raise SystemExit(bootstrap_store.verify())
    state = ROOT / '.buck2'
    with preparation_lock(state) as lock_fd:
        prepare_client(args, state, lock_fd)


if __name__ == '__main__':
    main()
