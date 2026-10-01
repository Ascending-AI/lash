import fcntl
import json
import os
from pathlib import Path
import subprocess
import signal
import time
import sys
import tempfile
import unittest

WATCHDOG = Path(__file__).resolve().parents[1] / 'test_timeout.py'


class WatchdogTests(unittest.TestCase):
    def test_inherited_database_slot_descriptor_survives(self):
        with tempfile.TemporaryDirectory() as work:
            work = Path(work)
            with (work / 'slot.lock').open('w') as slot:
                fcntl.flock(slot, fcntl.LOCK_EX)
                command = [sys.executable, str(WATCHDOG), sys.executable, '-c', 'import os,sys; os.fstat(int(sys.argv[1]))', str(slot.fileno())]
                result = subprocess.run(command, pass_fds=[slot.fileno()], env=dict(os.environ, LASH_TEST_TIMEOUT_SECONDS='5', TEST_UNDECLARED_OUTPUTS_DIR=str(work)))
                self.assertEqual(result.returncode, 0)
                metadata = json.loads((work / '_lash_runner/execution.json').read_text())
                self.assertFalse(metadata['timed_out'])

    def test_handled_term_reaps_grandchild_and_releases_database_slot(self):
        with tempfile.TemporaryDirectory() as work:
            work = Path(work)
            child = work / 'child.py'
            child.write_text("""import json, os, signal, subprocess, sys, time
from pathlib import Path
root = Path(sys.argv[1])
grandchild = subprocess.Popen([sys.executable, '-c',
    'import signal,time;from pathlib import Path;import sys;signal.signal(signal.SIGTERM,signal.SIG_IGN);Path(sys.argv[1]).write_text("ready");time.sleep(60)', str(root/'grandchild-ready')], close_fds=False)
def finish(number, frame):
    (root/'test.xml').write_text('<testsuite><testcase name="interrupted"/></testsuite>')
    sys.exit(0)
signal.signal(signal.SIGTERM, finish)
while not (root/'grandchild-ready').exists(): time.sleep(.01)
(root/'ready.json').write_text(json.dumps({'child':os.getpid(),'grandchild':grandchild.pid}))
time.sleep(60)
""")
            owned = None
            with (work / 'slot.lock').open('w') as slot:
                fcntl.flock(slot, fcntl.LOCK_EX)
                watchdog = subprocess.Popen([sys.executable, str(WATCHDOG), sys.executable, str(child), str(work)], pass_fds=[slot.fileno()], start_new_session=True, env=dict(os.environ, LASH_TEST_TIMEOUT_SECONDS='30', TEST_UNDECLARED_OUTPUTS_DIR=str(work)))
                slot.close()
                try:
                    deadline = time.monotonic() + 5
                    while not (work / 'ready.json').exists():
                        if time.monotonic() >= deadline or watchdog.poll() is not None:
                            self.fail('Test subprocess did not become ready')
                        time.sleep(.01)
                    owned = json.loads((work / 'ready.json').read_text())
                    os.kill(watchdog.pid, signal.SIGTERM)
                    self.assertEqual(watchdog.wait(timeout=8), 128 + signal.SIGTERM)
                    metadata = json.loads((work / '_lash_runner/execution.json').read_text())
                    self.assertEqual(metadata['exit_code'], 0)
                    self.assertEqual(metadata['interrupted_signal'], signal.SIGTERM)
                    self.assertTrue(metadata['cleanup_complete'])
                    self.assertIn('interrupted', (work / 'test.xml').read_text())
                    self.assertFalse(Path(f'/proc/{owned["grandchild"]}').exists())
                    with (work / 'slot.lock').open('a') as contender:
                        fcntl.flock(contender, fcntl.LOCK_EX | fcntl.LOCK_NB)
                finally:
                    if owned:
                        try:
                            os.killpg(owned['child'], signal.SIGKILL)
                        except ProcessLookupError:
                            pass
                    if watchdog.poll() is None:
                        watchdog.kill()
                        watchdog.wait()

    def test_deadline_preserves_metadata_and_exit_code(self):
        with tempfile.TemporaryDirectory() as work:
            result = subprocess.run([sys.executable, str(WATCHDOG), sys.executable, '-c', 'import time;time.sleep(10)'], env=dict(os.environ, LASH_TEST_TIMEOUT_SECONDS='.05', TEST_UNDECLARED_OUTPUTS_DIR=work))
            self.assertEqual(result.returncode, 124)
            metadata = json.loads((Path(work) / '_lash_runner/execution.json').read_text())
            self.assertTrue(metadata['timed_out'])


if __name__ == '__main__':
    unittest.main()
