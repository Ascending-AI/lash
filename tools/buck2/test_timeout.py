#!/usr/bin/env python3
"""Enforce test deadlines and reap the test's owned process group."""
import ctypes
import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import time


def signal_group(group, number):
    try:
        os.killpg(group, number)
    except ProcessLookupError:
        pass


def reap_group(group):
    """Reap descendants adopted after their immediate parent exits."""
    deadline = time.monotonic() + 5
    while True:
        try:
            child, _ = os.waitpid(-group, os.WNOHANG)
        except ChildProcessError:
            return True
        if child:
            continue
        if time.monotonic() >= deadline:
            return False
        time.sleep(.01)


def run(command):
    timeout = float(os.environ['LASH_TEST_TIMEOUT_SECONDS'])
    if timeout <= 0:
        raise ValueError('The test timeout must be positive')
    metadata = Path(os.environ['TEST_UNDECLARED_OUTPUTS_DIR']) / '_lash_runner' / 'execution.json'
    metadata.parent.mkdir(parents=True, exist_ok=True)
    environment = dict(os.environ)
    environment.pop('LASH_TEST_TIMEOUT_SECONDS', None)
    # Local service tests need orphaned grandchildren reaped before releasing a
    # PostgreSQL slot. This process owns no children outside the test group.
    libc = ctypes.CDLL(None, use_errno=True)
    if libc.prctl(36, 1, 0, 0, 0) != 0:  # PR_SET_CHILD_SUBREAPER (Linux)
        raise OSError(ctypes.get_errno(), 'Cannot supervise test descendants')
    started = time.monotonic()
    process = subprocess.Popen(command, env=environment, start_new_session=True, close_fds=False)
    interrupted = []

    def forward(number, frame):
        if not interrupted:
            interrupted.append((number, time.monotonic()))
        signal_group(process.pid, number)

    for number in (signal.SIGTERM, signal.SIGINT, signal.SIGHUP):
        signal.signal(number, forward)
    timed_out = False
    shutdown_deadline = None
    while process.poll() is None:
        now = time.monotonic()
        if interrupted:
            shutdown_deadline = interrupted[0][1] + 5
        elif now >= started + timeout and not timed_out:
            timed_out = True
            shutdown_deadline = now + 5
            signal_group(process.pid, signal.SIGTERM)
        if shutdown_deadline is not None and now >= shutdown_deadline:
            signal_group(process.pid, signal.SIGKILL)
            break
        try:
            process.wait(timeout=.05)
        except subprocess.TimeoutExpired:
            pass
    # A child may handle TERM and exit zero while a grandchild ignores it.
    # Kill the remaining owned group even after the direct child has exited.
    signal_group(process.pid, signal.SIGKILL)
    code = process.wait()
    cleanup_complete = reap_group(process.pid)
    interrupted_signal = interrupted[0][0] if interrupted else None
    metadata.write_text(json.dumps({'schema': 1, 'timed_out': timed_out, 'interrupted_signal': interrupted_signal, 'exit_code': code, 'cleanup_complete': cleanup_complete, 'elapsed_seconds': time.monotonic() - started}) + '\n')
    if interrupted_signal:
        return 128 + interrupted_signal
    if timed_out:
        return 124
    if not cleanup_complete:
        return 125
    return 128 - code if code < 0 else code


if __name__ == '__main__':
    raise SystemExit(run(sys.argv[1:]))
