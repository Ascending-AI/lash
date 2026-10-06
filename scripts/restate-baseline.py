#!/usr/bin/env python3
"""Run the FIG-5168 live baseline with private, durable PostgreSQL 16.

Build the binary with kiln first, then:
  kiln gate lash fig-5168 -- python3 scripts/restate-baseline.py \
    --binary <kiln-built-lash-perf> --out-dir .kiln/FIG-5168/live

Uses the same pinned Restate launcher as `just latency-gate`. PostgreSQL
keeps fsync, synchronous_commit and full_page_writes enabled. The Docker
container and Restate processes stop even if measurement fails. No service
configuration or counters belonging to another run are changed.
"""
import argparse
import json
import os
from pathlib import Path
import platform
import socket
import signal
import subprocess
import time


def command(argv, **kwargs):
    return subprocess.run(argv, check=True, text=True, **kwargs)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=Path, required=True)
    parser.add_argument('--out-dir', type=Path, required=True)
    parser.add_argument('--samples', type=int, default=10)
    parser.add_argument('--cases', default='')
    parser.add_argument('--postgres-max-connections', type=int, default=512)
    parser.add_argument('--wait-seconds', type=int, default=30)
    args = parser.parse_args()
    gate = os.environ['KILN_GATE_ID']
    root = Path(__file__).resolve().parents[1]
    output = args.out_dir.resolve()
    output.mkdir(parents=True, exist_ok=True)
    temporary = output / 'temporary'
    temporary.mkdir(exist_ok=True)
    env = dict(os.environ, TMPDIR=str(temporary))
    with socket.socket() as probe:
        probe.bind(('127.0.0.1', 0))
        port = probe.getsockname()[1]
    name = f'lash-baseline-{gate}'.lower().replace('_', '-')
    image = 'postgres:16-alpine@sha256:721873c34ceb9f8d8fc265984940dc982404c105f19ad51be9fdc5970a6080ea'
    command(['docker', 'run', '--detach', '--rm', '--name', name,
             '-e', 'POSTGRES_USER=lash', '-e', 'POSTGRES_PASSWORD=baseline',
             '-e', 'POSTGRES_DB=lash', '-p', f'127.0.0.1:{port}:5432',
             image, '-c', 'shared_preload_libraries=pg_stat_statements',
             '-c', f'max_connections={args.postgres_max_connections}', '-c', 'max_locks_per_transaction=256'])
    def terminate(signum, _frame):
        raise SystemExit(128 + signum)

    signal.signal(signal.SIGTERM, terminate)
    try:
        deadline = time.monotonic() + 90
        while subprocess.run(['docker', 'exec', name, 'pg_isready', '-h', '127.0.0.1', '-U', 'lash', '-d', 'lash'],
                             stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL).returncode:
            if time.monotonic() >= deadline:
                raise RuntimeError('private PostgreSQL did not start')
            time.sleep(.2)
        sql = ['docker', 'exec', '-i', name, 'psql', '-U', 'lash', '-d', 'lash', '-v', 'ON_ERROR_STOP=1']
        settings = command(sql + ['-Atc', 'SELECT version(); SHOW fsync; SHOW synchronous_commit; SHOW full_page_writes; SHOW max_connections;'],
                           capture_output=True).stdout
        image_id = command(['docker', 'image', 'inspect', image, '--format', '{{.Id}}'], capture_output=True).stdout.strip()
        restate_path = command(['python3', 'scripts/ci/restate_suite.py', 'server-path'], env=env, capture_output=True).stdout.strip()
        restate_version = command([restate_path, '--version'], capture_output=True).stdout.strip()
        info = {
            'gate': gate, 'source_sha': command(['git', 'rev-parse', 'HEAD'], capture_output=True).stdout.strip(),
            'machine': platform.uname()._asdict(), 'cpu': Path('/proc/cpuinfo').read_text(),
            'memory': Path('/proc/meminfo').read_text(), 'load_before': Path('/proc/loadavg').read_text(),
            'postgres_image': image_id, 'postgres_settings': settings,
            'restate_version': restate_version, 'binary': str(args.binary.resolve()),
            'samples': args.samples, 'wait_seconds': args.wait_seconds,
            'postgres_max_connections': args.postgres_max_connections,
        }
        (output / 'machine.json').write_text(json.dumps(info, indent=2) + '\n')
        env['LASH_POSTGRES_DATABASE_URL'] = f'postgres://lash:baseline@127.0.0.1:{port}/lash'
        invocation = ['python3', 'scripts/ci/restate_suite.py', 'serve', '--name', name,
                      '--keep-log', str(output / 'restate-server.log'), '--',
                      str(args.binary.resolve()), 'restate-baseline', '--out', str(output / 'samples.json'),
                      '--samples', str(args.samples), '--wait-seconds', str(args.wait_seconds)]
        if args.cases:
            invocation += ['--cases', args.cases]
        (output / 'command.json').write_text(json.dumps(invocation, indent=2) + '\n')
        with (output / 'measurement.log').open('w') as log:
            measurement = subprocess.Popen(invocation, env=env, stdout=log, stderr=subprocess.STDOUT, text=True)
            try:
                exit_code = measurement.wait()
            finally:
                if measurement.poll() is None:
                    measurement.terminate()
                    measurement.wait()
        print((output / 'measurement.log').read_text())
        info['load_after'] = Path('/proc/loadavg').read_text()
        info['exit_code'] = exit_code
        (output / 'machine.json').write_text(json.dumps(info, indent=2) + '\n')
        return exit_code
    finally:
        with (output / 'postgres.log').open('w') as log:
            subprocess.run(['docker', 'logs', name], stdout=log, stderr=subprocess.STDOUT)
        subprocess.run(['docker', 'stop', name], check=True, stdout=subprocess.DEVNULL)


if __name__ == '__main__':
    raise SystemExit(main())
