#!/usr/bin/env python3
"""Measure only the requested worktrees' live build coordinators and command.

Remote compiler/worker processes and unrelated daemons are excluded. Command
CPU includes client subprocesses; daemon CPU is reported separately. Samples
are passive /proc reads and never signal a process or reset a cache.
"""
from __future__ import annotations

import argparse
import json
import os
from pathlib import Path
import resource
import subprocess
import time

def proc(pid: int) -> dict | None:
    try:
        p = Path('/proc') / str(pid)
        values = (p / 'stat').read_text().rsplit(') ', 1)[1].split()
        args = (p / 'cmdline').read_bytes().replace(b'\0', b' ').decode(errors='replace')
        return {'pid': pid, 'cpu_ticks': int(values[11]) + int(values[12]),
                'start_ticks': int(values[19]), 'rss_bytes': int(values[21]) * os.sysconf('SC_PAGE_SIZE'),
                'args': args, 'cwd': str((p / 'cwd').resolve())}
    except (OSError, IndexError, ValueError):
        return None

def discover(roots: list[str]) -> list[dict]:
    rows = []
    for name in ('java', 'buck2'):
        r = subprocess.run(['pgrep', '-x', name], capture_output=True, text=True)
        for line in r.stdout.splitlines():
            row = proc(int(line))
            if row and ('daemon' in row['args'] or 'forkserver' in row['args'] or name == 'java') and any(
                    row['cwd'] == root or row['cwd'].startswith(root + '/buck-out/')
                    or ('--workspace_directory=' + root) in row['args']
                    for root in roots):
                rows.append(row)
    return rows

def main() -> int:
    p = argparse.ArgumentParser()
    p.add_argument('--repo', required=True, type=Path)
    p.add_argument('--extra-repo', action='append', type=Path, default=[])
    p.add_argument('--result', required=True, type=Path)
    p.add_argument('--log', required=True, type=Path)
    p.add_argument('command', nargs=argparse.REMAINDER)
    a = p.parse_args()
    command = a.command[1:] if a.command[:1] == ['--'] else a.command
    if not command:
        p.error('a command is required after --')
    roots = [str(x.resolve()) for x in [a.repo, *a.extra_repo]]
    ticks = os.sysconf('SC_CLK_TCK')
    initial = {r['pid']: r for r in discover(roots)}
    latest = dict(initial)
    cpu_baseline = {pid: r['cpu_ticks'] for pid, r in initial.items()}
    samples = []
    child_before = resource.getrusage(resource.RUSAGE_CHILDREN)
    start = time.monotonic()
    next_discover = start
    a.log.parent.mkdir(parents=True, exist_ok=True)
    with a.log.open('wb') as log:
        child = subprocess.Popen(command, cwd=a.repo, stdout=log, stderr=subprocess.STDOUT)
        while True:
            now = time.monotonic()
            if now >= next_discover:
                for row in discover(roots):
                    if row['pid'] not in latest:
                        cpu_baseline[row['pid']] = 0
                    latest[row['pid']] = row
                next_discover = now + 2
            live = []
            for pid, old in list(latest.items()):
                row = proc(pid)
                if row and row['start_ticks'] == old['start_ticks']:
                    latest[pid] = row
                    live.append(row)
            samples.append({'elapsed_s': now - start,
                            'total_daemon_rss_bytes': sum(r['rss_bytes'] for r in live),
                            'daemon_pids': [r['pid'] for r in live]})
            if child.poll() is not None:
                break
            time.sleep(0.25)
    wall = time.monotonic() - start
    child_after = resource.getrusage(resource.RUSAGE_CHILDREN)
    result = {'command': command, 'roots': roots, 'exit_code': child.returncode,
              'wall_seconds': wall,
              'client_cpu_seconds': child_after.ru_utime + child_after.ru_stime - child_before.ru_utime - child_before.ru_stime,
              'daemon_cpu_seconds': sum(max(0, row['cpu_ticks'] - cpu_baseline[pid]) / ticks for pid, row in latest.items()),
              'peak_total_daemon_rss_bytes': max((s['total_daemon_rss_bytes'] for s in samples), default=0),
              'end_total_daemon_rss_bytes': samples[-1]['total_daemon_rss_bytes'],
              'daemon_pids': sorted(latest), 'samples': samples,
              'limitations': ['Passive 250-ms RSS sampling may miss peaks.',
                              'Daemon CPU and client CPU are distinct and should not be summed without checking fork accounting.',
                              'Client CPU includes the small sampler discovery subprocesses.',
                              'Daemon process discovery repeats every two seconds.']}
    a.result.parent.mkdir(parents=True, exist_ok=True)
    a.result.write_text(json.dumps(result, indent=2) + '\n')
    print(json.dumps({k: v for k, v in result.items() if k not in ('samples', 'command', 'roots', 'limitations')}))
    return child.returncode

if __name__ == '__main__':
    raise SystemExit(main())
