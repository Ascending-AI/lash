"""Record the executor runtime and the action's own cgroup accounting."""
import json
import pathlib
import platform
import sys

root = pathlib.Path('/sys/fs/cgroup')
result = {'machine': platform.machine(), 'system': platform.system(), 'libc': platform.libc_ver(), 'hostname': platform.node(), 'proc_self_cgroup': pathlib.Path('/proc/self/cgroup').read_text(), 'limits': {}}
for name in ('cpu.max', 'memory.max', 'memory.current', 'memory.peak', 'cpu.stat', 'memory.events'):
    result['limits'][name] = (root / name).read_text().strip()
assert result['limits']['cpu.max'].split()[0] != 'max'
assert result['limits']['memory.max'] != 'max'
pathlib.Path(sys.argv[1]).write_text(json.dumps(result, indent=2) + '\n')
