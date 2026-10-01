#!/usr/bin/env python3
"""Prepare pinned Python wheels and Buck2's public test protocol privately."""
import fcntl
import hashlib
import json
import os
from pathlib import Path
import platform
import shutil
import subprocess
import sys
import tempfile
import urllib.request
import zipfile

HERE = Path(__file__).resolve().parent
ROOT = HERE.parents[1]


def digest(path):
    with path.open('rb') as source:
        return hashlib.file_digest(source, 'sha256').hexdigest()


def safe_directory(path, private=True):
    for parent in (path, *path.parents):
        if parent.is_symlink():
            raise RuntimeError(f'Refusing symlinked runner state: {parent}')
    path.mkdir(mode=0o700, parents=True, exist_ok=True)
    if private and (path.stat().st_uid != os.getuid() or path.stat().st_mode & 0o022):
        raise RuntimeError(f'Runner state is not private to its owner: {path}')
    return path


def fetch(pin, downloads):
    path = downloads / pin['sha256']
    if path.is_symlink():
        raise RuntimeError(f'Refusing symlinked download: {path}')
    if path.exists():
        if not path.is_file() or digest(path) != pin['sha256']:
            raise RuntimeError(f'Invalid cached runner input: {path}')
        return path
    with tempfile.NamedTemporaryFile(dir=downloads, delete=False) as output:
        temporary = Path(output.name)
        try:
            with urllib.request.urlopen(pin['url'], timeout=60) as source:
                shutil.copyfileobj(source, output)
            output.flush()
            if digest(temporary) != pin['sha256']:
                raise RuntimeError(f'Runner input checksum mismatch: {pin["filename"]}')
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)
    return path


def ensure_runtime():
    if platform.system() != 'Linux' or platform.machine() != 'x86_64' or sys.version_info[:2] not in ((3, 11), (3, 12), (3, 13)):
        raise RuntimeError('The pinned test runner requires Linux x86_64 and CPython 3.11, 3.12 or 3.13')
    pins = json.loads((HERE / 'runner-pins.json').read_text())
    tag = f'cp{sys.version_info.major}{sys.version_info.minor}'
    identity = hashlib.sha256(json.dumps({'pins': pins, 'python': tag, 'schema': 1}, sort_keys=True).encode()).hexdigest()
    state = safe_directory(ROOT / '.buck2' / 'test-runner')
    destination = state / identity
    fd = os.open(state / 'prepare.lock', os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    with os.fdopen(fd, 'r+') as lease:
        fcntl.flock(lease, fcntl.LOCK_EX)
        if destination.exists():
            safe_directory(destination)
            receipt = json.loads((destination / 'receipt.json').read_text())
            if receipt.get('identity') != identity:
                raise RuntimeError('Invalid test runner identity')
            for relative, expected in receipt['files'].items():
                path = destination / relative
                if path.is_symlink() or not path.is_file() or digest(path) != expected:
                    raise RuntimeError(f'Test runner input changed: {path}')
            return destination
        downloads = safe_directory(state / 'downloads')
        with tempfile.TemporaryDirectory(prefix='prepare-', dir=state) as work:
            work = Path(work)
            site = safe_directory(work / 'site')
            proto = safe_directory(work / 'proto')
            generated = safe_directory(work / 'generated')
            for package in pins['packages'].values():
                candidates = [w for w in package['wheels'] if f'-{tag}-{tag}-' in w['filename'] or '-abi3-' in w['filename'] or '-py3-none-any.' in w['filename']]
                if not candidates:
                    raise RuntimeError(f'No pinned wheel for {tag}: {package}')
                wheel = fetch(candidates[0], downloads)
                with zipfile.ZipFile(wheel) as archive:
                    for member in archive.infolist():
                        path = Path(member.filename)
                        if path.is_absolute() or '..' in path.parts or ((member.external_attr >> 16) & 0o170000) == 0o120000:
                            raise RuntimeError('Unsafe wheel entry')
                    archive.extractall(site)
            for pin in pins['protocols']:
                shutil.copyfile(fetch(pin, downloads), proto / pin['filename'])
            env = dict(os.environ, PYTHONPATH=str(site), PYTHONDONTWRITEBYTECODE='1')
            subprocess.run([sys.executable, '-m', 'grpc_tools.protoc', '-I' + str(proto), '-I' + str(site / 'grpc_tools' / '_proto'), '--python_out=' + str(generated), '--grpc_python_out=' + str(generated), *[str(p) for p in sorted(proto.glob('*.proto'))]], env=env, check=True)
            files = {str(p.relative_to(work)): digest(p) for p in sorted(work.rglob('*')) if p.is_file()}
            (work / 'receipt.json').write_text(json.dumps({'identity': identity, 'files': files}, indent=2) + '\n')
            os.replace(work, destination)
    return destination


def activate():
    runtime = ensure_runtime()
    sys.dont_write_bytecode = True
    sys.path[:0] = [str(runtime / 'site'), str(runtime / 'generated')]
    return runtime


if __name__ == '__main__':
    print(ensure_runtime())
