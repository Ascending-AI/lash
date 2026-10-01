#!/usr/bin/env python3
"""Run the same controlled workload in two named, owned worktrees."""
import argparse
import json
from pathlib import Path
import subprocess
import time

p = argparse.ArgumentParser()
p.add_argument('--repo', action='append', required=True, type=Path)
p.add_argument('--command', required=True)
p.add_argument('--result', required=True, type=Path)
a = p.parse_args()
jobs = []
for index, repo in enumerate(a.repo):
    log = a.result.with_name(a.result.stem + f'-{index}.log').open('wb')
    start = time.monotonic()
    child = subprocess.Popen(['bash', '-c', a.command], cwd=repo,
                             stdout=log, stderr=subprocess.STDOUT)
    jobs.append((repo, child, log, start))
rows = []
pending = list(jobs)
while pending:
    for item in list(pending):
        repo, child, log, start = item
        code = child.poll()
        if code is not None:
            log.close()
            rows.append({'repo': str(repo), 'wall_seconds': time.monotonic() - start,
                         'exit_code': code})
            pending.remove(item)
    if pending:
        time.sleep(0.05)
a.result.write_text(json.dumps({'workloads': rows,
                               'slowest_wall_seconds': max(r['wall_seconds'] for r in rows)},
                              indent=2) + '\n')
raise SystemExit(0 if all(r['exit_code'] == 0 for r in rows) else 1)
