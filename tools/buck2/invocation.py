"""Coordinate the stock daemon's startup-only remote execution configuration."""
from contextlib import contextmanager
import configparser
import fcntl
import hashlib
import json
import os
from pathlib import Path
import re
import signal
import stat
import subprocess
import sys
import tempfile
import time


def regular_file(path):
    fd = os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
    with os.fdopen(fd, 'r') as source:
        if not stat.S_ISREG(os.fstat(source.fileno()).st_mode):
            raise ValueError(f'Expected a regular file: {path}')
        return source.read()


def replace_limit(text, jobs):
    lines = text.splitlines(keepends=True)
    section = None
    positions = []
    insertion = None
    for index, line in enumerate(lines):
        match = re.fullmatch(r'\s*\[([^]]+)\]\s*(?:[#;].*)?\n?', line)
        if match:
            if section == 'buck2_re_client' and insertion is None:
                insertion = index
            section = match[1]
        elif section == 'buck2_re_client' and re.match(r'\s*execution_concurrency_limit\s*=', line):
            positions.append(index)
    if len(positions) > 1:
        raise ValueError('Duplicate remote execution limits in .buckconfig.local')
    setting = f'execution_concurrency_limit = {jobs}\n'
    if positions:
        lines[positions[0]] = setting
    elif insertion is not None:
        lines.insert(insertion, setting)
    elif section == 'buck2_re_client':
        if lines and not lines[-1].endswith('\n'):
            lines[-1] += '\n'
        lines.append(setting)
    else:
        lines.append('\n[buck2_re_client]\n' + setting)
    return ''.join(lines)


def atomic_write(path, text):
    if path.is_symlink():
        raise ValueError(f'Refusing symlinked configuration or receipt: {path}')
    with tempfile.NamedTemporaryFile(mode='w', dir=path.parent, delete=False) as output:
        temporary = Path(output.name)
        try:
            output.write(text)
            output.flush()
            os.fsync(output.fileno())
            os.replace(temporary, path)
        finally:
            temporary.unlink(missing_ok=True)


def configuration(root, jobs):
    local = root / '.buckconfig.local'
    previous = regular_file(local) if local.exists() or local.is_symlink() else ''
    desired = replace_limit(previous, jobs)
    tracked = regular_file(root / '.buckconfig')
    identity = hashlib.sha256((tracked + '\0' + desired).encode())
    values = configparser.ConfigParser(interpolation=None, strict=False)
    try:
        values.read_string(tracked + '\n' + desired)
    except configparser.Error:
        raise ValueError('Cannot parse on-disk Buck2 configuration for daemon admission') from None
    for key in ('tls_client_cert', 'tls_ca_certs'):
        path = values.get('buck2_re_client', key, fallback=None)
        if path:
            credential = Path(path)
            if not credential.is_absolute():
                credential = root / credential
            identity.update(regular_file(credential).encode())
    return previous, desired, identity.hexdigest()


@contextmanager
def lock(path):
    fd = os.open(path, os.O_RDWR | os.O_CREAT | os.O_NOFOLLOW, 0o600)
    try:
        info = os.fstat(fd)
        if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o022:
            raise ValueError(f'Unsafe invocation lock: {path}')
        yield fd
    finally:
        os.close(fd)


def daemon_status(executable, root, isolation):
    result = subprocess.run([str(executable), '--isolation-dir', isolation, 'status'], cwd=root, capture_output=True, text=True, check=True, timeout=15)
    if not result.stdout.strip() and result.stderr.strip() == 'no buckd running':
        confirm_daemon_absent(executable, root, isolation)
        return None
    try:
        status = json.loads(result.stdout)
        if Path(status['project_root']).resolve() != root.resolve() or status['isolation_dir'] != isolation or not isinstance(status['active_commands'], list):
            raise ValueError()
    except (ValueError, KeyError, TypeError):
        raise ValueError('Cannot establish the requested Buck2 daemon identity and activity') from None
    return status


def confirm_daemon_absent(executable, root, isolation):
    # Stock status uses the same message for absence and any connection error.
    result = subprocess.run([str(executable), '--isolation-dir', isolation, 'root', '--kind', 'daemon'], cwd=root, capture_output=True, text=True, check=True, timeout=15)
    directory = Path(result.stdout.strip())
    if not directory.is_absolute() or directory.name != isolation:
        raise ValueError('Cannot establish Buck2 daemon metadata location')
    for parent in (directory, *directory.parents):
        if parent.is_symlink():
            raise ValueError('Refusing symlinked Buck2 daemon metadata')
    try:
        info = directory.stat()
    except FileNotFoundError:
        return
    if not stat.S_ISDIR(info.st_mode) or info.st_uid != os.getuid() or info.st_mode & 0o002:
        raise ValueError('Unsafe Buck2 daemon metadata directory')
    lifecycle = None
    try:
        try:
            lifecycle = os.open(directory / 'buckd.lifecycle', os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK)
        except FileNotFoundError:
            pass
        if lifecycle is not None:
            info = os.fstat(lifecycle)
            if not stat.S_ISREG(info.st_mode) or info.st_uid != os.getuid():
                raise ValueError('Unsafe Buck2 daemon lifecycle file')
            try:
                fcntl.flock(lifecycle, fcntl.LOCK_EX | fcntl.LOCK_NB)
            except BlockingIOError:
                raise ValueError('Buck2 daemon lifecycle is busy; cannot establish absence') from None
        pids = set()
        for name in ('buckd.pid', 'buckd.info'):
            try:
                contents = regular_file(directory / name)
            except FileNotFoundError:
                continue
            try:
                pid = int(contents.strip()) if name == 'buckd.pid' else json.loads(contents)['pid']
                if type(pid) is not int or not 1 < pid <= 2147483647:
                    raise ValueError()
            except (ValueError, KeyError, TypeError):
                raise ValueError('Cannot establish Buck2 daemon PID from metadata') from None
            pids.add(pid)
        for pid in pids:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                continue
            except PermissionError:
                pass
            raise ValueError('Buck2 status failed while a recorded daemon PID survives; refusing configuration changes or clean')
    finally:
        if lifecycle is not None:
            os.close(lifecycle)


def watch_aliases(root):
    """Return the root's Bazel convenience links; refuse any other link back into the project.

    Buck2's watcher follows directory symlinks when it starts. A link that
    leads back into the project gives a source directory a second name under
    one inotify watch: its changes are reported under the link, and removing
    the link drops the watch. Either way the daemon keeps building the old
    sources until it restarts.
    """
    project = root.resolve()

    def inside(path):
        target = path.resolve()
        return target == project or project in target.parents

    aliases = []
    for entry in os.scandir(root):
        if not entry.is_symlink() or not entry.is_dir():
            continue
        path = Path(entry.path)
        if entry.name.startswith('bazel-'):
            aliases.append(path)
        elif inside(path) or any(child.is_symlink() and inside(child) for child in path.iterdir()):
            raise ValueError(f'{entry.name} is a symlink leading back into the project, which makes Buck2 miss source changes; remove it')
    return aliases


def acquire(fd, mode, cancelled):
    while True:
        if cancelled():
            raise InterruptedError('Invocation cancelled while waiting for admission')
        try:
            fcntl.flock(fd, mode | fcntl.LOCK_NB)
            return
        except BlockingIOError:
            time.sleep(0.05)


@contextmanager
def admission(root, executable, isolation, jobs, exclusive=False, cancelled=lambda: False):
    from runner_bootstrap import safe_directory
    if not re.fullmatch(r'[A-Za-z0-9_-]+', isolation) or jobs <= 0:
        raise ValueError('Invalid Buck2 isolation directory or concurrency limit')
    state = safe_directory(root / '.buck2/invocations')
    local = root / '.buckconfig.local'
    receipt_path = state / (isolation + '.json')
    with lock(state / 'admission.lock') as gate, lock(state / 'active.lock') as active:
        acquire(gate, fcntl.LOCK_EX, cancelled)
        previous, desired, identity = configuration(root, jobs)
        receipt = json.loads(regular_file(receipt_path)) if receipt_path.exists() or receipt_path.is_symlink() else {}
        aliases = watch_aliases(root)
        transition = previous != desired or receipt.get('configuration') != identity or bool(aliases)
        acquire(active, fcntl.LOCK_EX if transition or exclusive else fcntl.LOCK_SH, cancelled)
        if transition or exclusive:
            # A fork refresh may replace credentials while earlier clients drain.
            previous, desired, identity = configuration(root, jobs)
            status = daemon_status(executable, root, isolation)
            if status is not None and status['active_commands']:
                raise ValueError('Buck2 has active commands outside the managed driver; wait for them before changing --jobs or executor configuration')
        if transition:
            if previous != desired:
                atomic_write(local, desired)
            for alias in aliases:
                alias.unlink(missing_ok=True)
            if status is not None:
                subprocess.run([str(executable), '--isolation-dir', isolation, 'kill'], cwd=root, check=True)
            atomic_write(receipt_path, json.dumps({'configuration': identity, 'jobs': jobs}) + '\n')
        if not exclusive:
            fcntl.flock(active, fcntl.LOCK_SH)
        fcntl.flock(gate, fcntl.LOCK_UN)
        yield


def run_command(root, jobs, isolation, argv, planner=None, **kwargs):
    """An orphaned guardian keeps the lease until its Buck client exits."""
    sys.stdout.flush()
    sys.stderr.flush()
    guardian = os.fork()
    if guardian == 0:
        child = None
        interrupted = 0

        def forward(signum, _frame):
            nonlocal interrupted
            interrupted = signum
            if child is not None and child.poll() is None:
                child.send_signal(signum)

        try:
            os.setsid()
            for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
                signal.signal(signum, forward)
            with admission(root, argv[0], isolation, jobs, exclusive=argv[3] == 'clean', cancelled=lambda: interrupted):
                if interrupted:
                    os._exit(128 + interrupted)
                if planner is not None:
                    argv = planner(argv)
                if interrupted:
                    os._exit(128 + interrupted)
                # Never pass the lease FD: Buck's daemon inherits arbitrary FDs.
                child = subprocess.Popen(argv, cwd=root, close_fds=True, **kwargs)
                if interrupted:
                    child.send_signal(interrupted)
                code = child.wait()
                os._exit(128 + interrupted if interrupted else code if code >= 0 else 128 - code)
        except BaseException as error:
            print(f'hermetic-build: {error}', file=sys.stderr, flush=True)
            os._exit(128 + interrupted if interrupted else 2)
    saved = {}

    def relay(signum, _frame):
        try:
            os.kill(guardian, signum)
        except ProcessLookupError:
            pass

    try:
        for signum in (signal.SIGINT, signal.SIGTERM, signal.SIGHUP):
            saved[signum] = signal.signal(signum, relay)
        _, status = os.waitpid(guardian, 0)
        code = os.waitstatus_to_exitcode(status)
        return code if code >= 0 else 128 - code
    finally:
        for signum, handler in saved.items():
            signal.signal(signum, handler)
