"""Real-process checks for preparation ownership across competing clients."""
import hashlib
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import time
import unittest

BOOTSTRAP = Path(__file__).resolve().parents[1] / 'bootstrap.py'

CHILD = '''import os, time
from pathlib import Path
root = Path.cwd()
tag = os.environ['PROBE_TAG']
with (root / 'events').open('a') as out: out.write(tag + ':begin\\n')
(root / (tag + '.ready')).write_text(str(os.getpid()))
if tag == 'first':
    deadline = time.monotonic() + 5
    while not (root / 'release').exists():
        if time.monotonic() > deadline: raise SystemExit('probe release deadline')
        time.sleep(0.01)
with (root / 'events').open('a') as out: out.write(tag + ':overlay_done\\n')
'''
FINISH = '''import os
from pathlib import Path
with Path('events').open('a') as out: out.write(os.environ['PROBE_TAG'] + ':end\\n')
'''
VENDOR = '''import os
from pathlib import Path
if not os.environ.get('KILN_REAL_CARGO'):
    raise SystemExit('vendor bootstrap requires KILN_REAL_CARGO')
with Path('events').open('a') as out: out.write(os.environ['PROBE_TAG'] + ':vendor\\n')
Path('vendor').mkdir(exist_ok=True)
'''


class BootstrapLockTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        tools = self.root / 'tools/buck2'
        tools.mkdir(parents=True)
        shutil.copyfile(BOOTSTRAP, tools / 'bootstrap.py')
        (tools / 'prelude_overlay.py').write_text(CHILD)
        (tools / 'bootstrap_rust_toolchain.py').write_text(FINISH)
        (tools / 'bootstrap_vendor.py').write_text(VENDOR)
        binary = self.root / '.buck2/bin/buck2'
        binary.parent.mkdir(parents=True)
        binary.write_bytes(b'private bootstrap fixture')
        (tools / 'pins.json').write_text(json.dumps({'buck2': {'executable_sha256': hashlib.sha256(binary.read_bytes()).hexdigest()}}))
        self.processes = []
        self.addCleanup(self.stop_processes)

    def stop_processes(self):
        (self.root / 'release').touch()
        for process in self.processes:
            try:
                process.communicate(timeout=5)
            except subprocess.TimeoutExpired:
                process.kill()
                process.communicate(timeout=5)

    def start(self, tag):
        process = subprocess.Popen([sys.executable, str(self.root / 'tools/buck2/bootstrap.py')], cwd=self.root, env=dict(os.environ, PROBE_TAG=tag, KILN_REAL_CARGO=sys.executable), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
        self.processes.append(process)
        return process

    def wait_ready(self, tag):
        deadline = time.monotonic() + 5
        while not (self.root / (tag + '.ready')).exists():
            if time.monotonic() > deadline:
                self.fail('bootstrap child did not start')
            time.sleep(0.01)

    def finish(self, process):
        stdout, stderr = process.communicate(timeout=5)
        self.assertEqual(process.returncode, 0, stdout + stderr)

    def test_competing_preparations_serialize_all_tool_phases(self):
        first = self.start('first')
        self.wait_ready('first')
        second = self.start('second')
        time.sleep(0.1)
        self.assertIsNone(second.poll())
        self.assertFalse((self.root / 'second.ready').exists())
        (self.root / 'release').touch()
        self.finish(first)
        self.finish(second)
        self.assertEqual((self.root / 'events').read_text().splitlines(), ['first:begin', 'first:overlay_done', 'first:end', 'first:vendor', 'second:begin', 'second:overlay_done', 'second:end', 'second:vendor'])

    def test_running_child_keeps_lock_after_parent_dies_then_releases_it(self):
        first = self.start('first')
        self.wait_ready('first')
        first.kill()
        first.wait(timeout=5)
        second = self.start('second')
        time.sleep(0.1)
        self.assertIsNone(second.poll())
        self.assertFalse((self.root / 'second.ready').exists())
        (self.root / 'release').touch()
        self.finish(second)
        first.communicate(timeout=5)
        self.assertEqual((self.root / 'events').read_text().splitlines(), ['first:begin', 'first:overlay_done', 'second:begin', 'second:overlay_done', 'second:end', 'second:vendor'])

    def test_fresh_cache_prepares_vendor_before_graph_checks(self):
        process = self.start('second')
        self.finish(process)
        self.assertTrue((self.root / 'vendor').is_dir())
        self.assertIn('second:vendor', (self.root / 'events').read_text().splitlines())

    def test_symlinked_lock_is_rejected_without_touching_its_target(self):
        target = self.root / 'unrelated'
        target.write_text('preserve')
        (self.root / '.buck2/prepare.lock').symlink_to(target)
        process = self.start('second')
        _, stderr = process.communicate(timeout=5)
        self.assertNotEqual(process.returncode, 0, stderr)
        self.assertEqual(target.read_text(), 'preserve')
        self.assertFalse((self.root / 'second.ready').exists())


if __name__ == '__main__':
    unittest.main()
