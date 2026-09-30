#!/usr/bin/env python3
"""Install the pinned official executable into this checkout's ignored state."""
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


def prepare_tools(executable, lock_fd):
    overlay = ROOT / 'tools/buck2/prelude_overlay.py'
    if overlay.is_file():
        subprocess.run([sys.executable, str(overlay), '--buck2', str(executable), '--prelude-dir', str(ROOT / '.buck2/prelude')], check=True, cwd=ROOT, stdout=subprocess.DEVNULL, pass_fds=(lock_fd,))
    for name in ('bootstrap_rust_toolchain.py', 'bootstrap_native_tools.py', 'bootstrap_reindeer.py'):
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
    with tempfile.TemporaryDirectory(prefix='bootstrap-', dir=state) as work:
        work = Path(work)
        archive = work / 'buck2.zst'
        if args.archive:
            shutil.copyfile(args.archive, archive)
        else:
            with urllib.request.urlopen(PIN['url'], timeout=60) as source, archive.open('wb') as output:
                shutil.copyfileobj(source, output)
        if digest(archive) != PIN['archive_sha256']:
            raise SystemExit('Buck2 release archive checksum mismatch')
        binary = work / 'buck2'
        subprocess.run(['zstd', '-q', '-d', str(archive), '-o', str(binary)], check=True, pass_fds=(lock_fd,))
        if digest(binary) != PIN['executable_sha256']:
            raise SystemExit('Buck2 executable checksum mismatch')
        binary.chmod(0o755)
        os.replace(binary, executable)
    if not args.client_only:
        prepare_tools(executable, lock_fd)
    print(executable)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--archive', type=Path, help='Use an already downloaded, checksum-verified release archive.')
    parser.add_argument('--client-only', action='store_true', help='Prepare only the official client, for checkout cleanup.')
    args = parser.parse_args()
    state = ROOT / '.buck2'
    with preparation_lock(state) as lock_fd:
        prepare_client(args, state, lock_fd)


if __name__ == '__main__':
    main()
