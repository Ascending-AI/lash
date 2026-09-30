#!/usr/bin/env python3
"""Read a worker and its descendants; cgroup memory is the deployment total."""
import argparse
import json
import os
from pathlib import Path


def process(path):
    text = (path / 'stat').read_text()
    fields = text[text.rindex(')') + 2:].split()
    status = dict(line.split(':', 1) for line in (path / 'status').read_text().splitlines() if ':' in line)
    return {'pid': int(path.name), 'parent_pid': int(fields[1]), 'epoch': fields[19],
            'cpu_ticks': int(fields[11]) + int(fields[12]),
            'rss_bytes': int(status.get('VmRSS', '0 kB').split()[0]) * 1024,
            'peak_rss_bytes': int(status.get('VmHWM', '0 kB').split()[0]) * 1024}


def descendants(rows, root):
    owned = {root}
    while True:
        more = {row['pid'] for row in rows if row['parent_pid'] in owned}
        if more <= owned:
            return [row for row in rows if row['pid'] in owned]
        owned |= more


def snapshot(root, proc=Path('/proc'), cgroup=Path('/sys/fs/cgroup')):
    rows = []
    for path in proc.iterdir():
        if path.name.isdecimal() and int(path.name) != os.getpid():
            try:
                rows.append(process(path))
            except (FileNotFoundError, ProcessLookupError):
                pass  # A process can exit between the directory and stat reads.
    rows = descendants(rows, root)
    if not any(row['pid'] == root for row in rows):
        raise ValueError('worker parent vanished while sampling')
    def pairs(name):
        return {key: int(value) for key, value in (line.split() for line in (cgroup / name).read_text().splitlines())}
    return {'generation': os.environ.get('WORKER_GENERATION'), 'processes': rows, 'clock_ticks_per_second': os.sysconf('SC_CLK_TCK'),
            'rss_sum_bytes': sum(row['rss_bytes'] for row in rows),
            'cpu_ticks_sum': sum(row['cpu_ticks'] for row in rows),
            'cgroup_memory_bytes': int((cgroup / 'memory.current').read_text()),
            'cgroup_peak_bytes': int((cgroup / 'memory.peak').read_text()),
            'cgroup_cpu': pairs('cpu.stat'), 'cgroup_events': pairs('memory.events'),
            'pool': {'status': 'PENDING', 'dependencies': ['FIG-4161', 'FIG-4162']}}


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('--root', type=int, required=True)
    print(json.dumps(snapshot(parser.parse_args().root)))
