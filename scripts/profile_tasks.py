#!/usr/bin/env python3
"""Pair tracing-off/on user instructions for the existing concurrent replay.

The executable's headless Console API client attaches over a private UDS and
writes task/source statistics. An installed tokio-console can attach to the
same socket while the replay runs: tokio-console file://localhost<socket>.
"""
import argparse
import fcntl
import struct
import termios
import json
import hashlib
import os
import pty
import shutil
import threading
import time
from pathlib import Path
import resource
import subprocess

from perf_artifacts import artifacts, runtime_label


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--turns', type=int, default=2)
    parser.add_argument('--console-bin', default=shutil.which('tokio-console'))
    parser.add_argument('--resume', action='store_true', help='Keep a completed tracing-off receipt after a failed tracing-on build.')
    args = parser.parse_args()
    if not args.console_bin:
        parser.error('tokio-console must be installed or supplied with --console-bin')
    root = Path(__file__).resolve().parents[1]
    out = args.out.resolve()
    out.mkdir(mode=0o700, parents=True, exist_ok=args.resume)
    resource.setrlimit(resource.RLIMIT_AS, (8 * 1024**3, 8 * 1024**3))
    counts = {}
    for mode in ('off', 'on'):
        if args.resume and mode == 'off' and (out/'off.json').is_file():
            receipt = json.loads((out/'off.json').read_text())
            workload = receipt['workloads'][0]
            if (workload['workload'], workload['sessions'], workload['turns'], len(workload['rounds'])) != ('concurrent-16',16,args.turns,args.turns):
                raise RuntimeError('completed control receipt has a different workload')
            label = runtime_label([])
            binary = artifacts(root,[label],build=False,report=out/'off.build.json',optimized=True,symbolized=True)[label]
            with binary.open('rb') as executable:
                digest = hashlib.file_digest(executable,'sha256').hexdigest()
            if digest != receipt['identity']['executable_sha256']:
                raise RuntimeError('completed control executable changed')
            row = next(line.split(';') for line in (out/'off.stat').read_text().splitlines() if ';instructions:u;' in line)
            counts[mode] = int(row[0])
            continue
        label = runtime_label(['console'] if mode == 'on' else [])
        report = out / f'{mode}.build.json'
        command = ['kiln', 'build', '--target-platforms', '//tools/buck2:profiling', '-c', 'kiln.rust_profile=optimized']
        if mode == 'on':
            command += ['-c', 'kiln.console=true']
        subprocess.run(command + [label, '--materializations', 'final', '--build-report', str(report)], cwd=root, check=True)
        binary = artifacts(root,[label],build=False,report=report,optimized=True,symbolized=True)[label]
        environment = dict(os.environ)
        environment.pop('LASH_CONSOLE_SOCKET', None)
        environment.pop('LASH_CONSOLE_OUT', None)
        if mode == 'on':
            environment.update(LASH_CONSOLE_SOCKET=str(out / 'tasks.sock'), LASH_CONSOLE_OUT=str(out / 'console.json'))
        command = ['perf','stat','-e','instructions:u','-x',';', '-o',str(out/f'{mode}.stat'), '--',
            str(binary),'replay','concurrent-16','--turns',str(args.turns),
            '--store-dir',str(out/f'{mode}-stores'),'--out',str(out/f'{mode}.json')]
        with (out/f'{mode}.stdout').open('w') as stdout, (out/f'{mode}.stderr').open('w') as stderr:
            if mode == 'off':
                subprocess.run(command,cwd=root,env=environment,stdout=stdout,stderr=stderr,check=True)
            else:
                with subprocess.Popen(command,cwd=root,env=environment,stdout=stdout,stderr=stderr) as process:
                    deadline = time.monotonic() + 12
                    while not (out/'tasks.sock').exists():
                        if process.poll() is not None or time.monotonic() >= deadline:
                            process.terminate()
                            raise RuntimeError('Console socket did not become ready')
                        time.sleep(0.01)
                    master, slave = pty.openpty()
                    fcntl.ioctl(slave, termios.TIOCSWINSZ, struct.pack("HHHH", 40, 160, 0, 0))
                    console_env = dict(os.environ, TERM='xterm-256color', COLORTERM='truecolor')
                    with subprocess.Popen([args.console_bin, 'file://localhost'+str(out/'tasks.sock')],
                            stdin=slave,stdout=slave,stderr=slave,env=console_env) as console:
                        os.close(slave)
                        def drain():
                            with (out/'tokio-console.terminal').open('wb') as terminal:
                                while True:
                                    try:
                                        chunk = os.read(master, 65536)
                                    except OSError:
                                        break
                                    if not chunk:
                                        break
                                    terminal.write(chunk)
                        reader = threading.Thread(target=drain)
                        reader.start()
                        try:
                            process.wait(timeout=300)
                            if process.returncode:
                                raise RuntimeError('instrumented replay failed: '+str(process.returncode))
                            if console.poll() is not None:
                                raise RuntimeError('tokio-console exited before the replay completed')
                        finally:
                            console.terminate()
                            console.wait(timeout=5)
                            reader.join(timeout=5)
                            os.close(master)
                            if process.poll() is None:
                                process.kill()
                                process.wait()
        row = next(line.split(';') for line in (out/f'{mode}.stat').read_text().splitlines() if ';instructions:u;' in line)
        counts[mode] = int(row[0])
    summary = {'kind':'lash.console-cost', 'workload':'concurrent-16', 'sessions':16, 'turns_per_session':args.turns,
        'instructions_u':counts, 'tracing_on_change_percent':100*(counts['on']/counts['off']-1),
        'build':'optimized symbolized frame-pointer platform for both builds', 'tokio_console':args.console_bin, 'scope':'whole invocation including setup, Console server/embedded receipt client and final two publication seconds; external TUI instructions excluded; one ordered shared-host pair, not quiet-host wall cost'}
    (out/'cost.json').write_text(json.dumps(summary,indent=2)+'\n')
    print(json.dumps(summary,indent=2))


if __name__ == '__main__':
    main()
