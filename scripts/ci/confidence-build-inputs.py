#!/usr/bin/env python3
"""Retain the verified checkout timestamps consumed by Cargo fingerprints."""
import hashlib
import json
import os
from pathlib import Path
import subprocess
import sys


def digest(path):
    return hashlib.sha256(path.read_bytes()).hexdigest()


def snapshot(destination):
    names = subprocess.check_output(['git', 'ls-files', '-z']).split(b'\0')
    inputs = {}
    for name in names:
        if not name:
            continue
        path = Path(os.fsdecode(name))
        if path.is_file():
            inputs[str(path)] = {'mtime_ns': path.stat().st_mtime_ns, 'sha256': digest(path)}
    destination.write_text(json.dumps(inputs) + '\n')


def restore(source):
    inputs = json.loads(source.read_text())
    # Validate the entire checkout before changing any timestamp. A changed
    # source must never be made to appear older than its compiled fingerprint.
    for name, expected in inputs.items():
        path = Path(name)
        if path.is_absolute() or '..' in path.parts or not path.is_file():
            raise ValueError(f'shared build input is unavailable: {name}')
        if digest(path) != expected['sha256']:
            raise ValueError(f'shared build input differs from the producer: {name}')
    for name, expected in inputs.items():
        path = Path(name)
        os.utime(path, ns=(path.stat().st_atime_ns, expected['mtime_ns']))
    print(f'restored {len(inputs)} verified shared build input timestamps')


if __name__ == '__main__':
    action, manifest = sys.argv[1:]
    {'snapshot': snapshot, 'restore': restore}[action](Path(manifest))
