import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

HERE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HERE))
import invocation

FAKE = r'''#!/usr/bin/env python3
import configparser, json, os
from pathlib import Path
import signal, sys, time
root = Path.cwd()
operation = sys.argv[3]
state = root / 'daemon.json'
if operation == 'status':
    if state.exists() and not (root / 'mask-status').exists():
        active = ['unmanaged'] if (root / 'unmanaged').exists() else []
        print(json.dumps({'project_root': str(root), 'isolation_dir': sys.argv[2], 'active_commands': active}))
    else:
        print('no buckd running', file=sys.stderr)
elif operation == 'root':
    print(root / 'daemon-metadata' / sys.argv[2])
elif operation == 'kill':
    if any(not p.with_suffix('.done').exists() for p in root.glob('*.start')) or (root / 'unmanaged').exists():
        raise SystemExit('attempted active daemon kill')
    with (root / 'kills').open('a') as output:
        output.write('kill\n')
    state.unlink(missing_ok=True)
else:
    token = sys.argv[4]
    if not state.exists():
        config = configparser.ConfigParser()
        config.read(root / '.buckconfig.local')
        state.write_text(json.dumps({'jobs': config.getint('buck2_re_client', 'execution_concurrency_limit')}))
    fds = []
    for path in Path('/proc/self/fd').iterdir():
        try:
            fds.append(str(path.resolve(strict=True)))
        except FileNotFoundError:
            pass
    data = json.loads(state.read_text())
    data['lease_inherited'] = any(path.endswith('active.lock') for path in fds)
    if token == 'handles-term':
        def terminate(signum, frame):
            (root / (token + '.done')).write_text('completed-output')
            raise SystemExit(0)
        signal.signal(signal.SIGTERM, terminate)
    (root / (token + '.start')).write_text(json.dumps(data))
    while not (root / (token + '.release')).exists():
        time.sleep(.02)
    (root / (token + '.done')).touch()
'''

WORKER = '''import sys
from pathlib import Path
sys.path.insert(0, sys.argv[1])
from invocation import run_command
root = Path(sys.argv[2])
raise SystemExit(run_command(root, int(sys.argv[3]), 'kiln', [str(root / 'buck2'), '--isolation-dir', 'kiln', 'build', sys.argv[4]]))
'''


class InvocationTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.root = self.make_root(Path(self.temporary.name) / 'one')
        self.processes = []

    def tearDown(self):
        for root, token, process in self.processes:
            (root / (token + '.release')).touch()
        for root, token, process in self.processes:
            process.wait(timeout=10)
        for root, token, process in self.processes:
            if (root / (token + '.start')).exists():
                self.wait_for(root / (token + '.done'))
        self.temporary.cleanup()

    def make_root(self, path):
        path.mkdir()
        (path / '.buckconfig').write_text('[cells]\nroot = .\n')
        (path / '.buckconfig.local').write_text('[buck2_re_client]\nengine_address = private-endpoint\n')
        executable = path / 'buck2'
        executable.write_text(FAKE)
        executable.chmod(0o700)
        metadata = path / 'daemon-metadata/kiln'
        metadata.mkdir(parents=True)
        (metadata / 'buckd.lifecycle').touch()
        return path

    def start(self, token, jobs, root=None):
        root = root or self.root
        process = subprocess.Popen([sys.executable, '-c', WORKER, str(HERE), str(root), str(jobs), token], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.processes.append((root, token, process))
        return process

    def wait_for(self, path):
        deadline = time.monotonic() + 10
        while not path.exists():
            if time.monotonic() >= deadline:
                self.fail(f'Timed out waiting for {path.name}')
            time.sleep(.02)

    def started(self, token, root=None):
        path = (root or self.root) / (token + '.start')
        self.wait_for(path)
        return json.loads(path.read_text())

    def release(self, token, root=None):
        (root or self.root).joinpath(token + '.release').touch()

    def test_matching_limits_overlap_and_transition_waits_for_both(self):
        a = self.start('a', 2)
        self.assertEqual(self.started('a')['jobs'], 2)
        b = self.start('b', 2)
        self.assertFalse(self.started('b')['lease_inherited'])
        c = self.start('c', 1)
        time.sleep(.2)
        self.assertFalse((self.root / 'c.start').exists())
        self.release('a')
        self.assertEqual(a.wait(timeout=10), 0)
        time.sleep(.2)
        self.assertFalse((self.root / 'c.start').exists())
        self.release('b')
        self.assertEqual(b.wait(timeout=10), 0)
        self.assertEqual(self.started('c')['jobs'], 1)
        self.release('c')
        self.assertEqual(c.wait(timeout=10), 0)
        self.assertEqual((self.root / 'kills').read_text(), 'kill\n')
        self.assertIn('engine_address = private-endpoint', (self.root / '.buckconfig.local').read_text())

    def test_planning_holds_transition_lease_until_client_finishes(self):
        worker = WORKER.replace(
            "raise SystemExit(run_command(",
            "import time\ndef plan(argv):\n    (root / 'planned').touch()\n    while not (root / 'plan.release').exists(): time.sleep(.02)\n    return argv\nraise SystemExit(run_command(",
        ).replace("sys.argv[4]]))", "sys.argv[4]], planner=plan))")
        process = subprocess.Popen([sys.executable, '-c', worker, str(HERE), str(self.root), '2', 'planned-client'], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
        self.processes.append((self.root, 'planned-client', process))
        try:
            self.wait_for(self.root / 'planned')
            transition = self.start('transition', 1)
            time.sleep(.2)
            self.assertFalse((self.root / 'transition.start').exists())
            (self.root / 'plan.release').touch()
            self.assertEqual(self.started('planned-client')['jobs'], 2)
            self.assertFalse((self.root / 'transition.start').exists())
            self.release('planned-client')
            self.assertEqual(process.wait(timeout=10), 0)
            self.assertEqual(self.started('transition')['jobs'], 1)
            self.release('transition')
            self.assertEqual(transition.wait(timeout=10), 0)
        finally:
            (self.root / 'plan.release').touch()

    def test_driver_death_keeps_lease_and_other_forks_are_independent(self):
        a = self.start('a', 2)
        self.started('a')
        a.kill()
        a.wait(timeout=10)
        b = self.start('b', 1)
        other = self.make_root(Path(self.temporary.name) / 'two')
        independent = self.start('independent', 1, other)
        self.assertEqual(self.started('independent', other)['jobs'], 1)
        self.assertFalse((self.root / 'b.start').exists())
        self.release('independent', other)
        self.assertEqual(independent.wait(timeout=10), 0)
        self.release('a')
        self.assertEqual(self.started('b')['jobs'], 1)
        self.release('b')
        self.assertEqual(b.wait(timeout=10), 0)

    def test_unmanaged_activity_rejects_transition_without_mutation(self):
        (self.root / 'daemon.json').write_text('{}')
        (self.root / 'unmanaged').touch()
        original = (self.root / '.buckconfig.local').read_bytes()
        process = self.start('refused', 1)
        self.assertEqual(process.wait(timeout=10), 2)
        self.assertEqual((self.root / '.buckconfig.local').read_bytes(), original)
        self.assertFalse((self.root / 'kills').exists())
        self.assertFalse((self.root / 'refused.start').exists())

    def test_credential_change_waits_and_does_not_overwrite_refresh(self):
        credential = self.root / 'identity.pem'
        credential.write_text('first-private-identity')
        with (self.root / '.buckconfig.local').open('a') as output:
            output.write('tls_client_cert = ' + str(credential) + '\n')
        a = self.start('a', 2)
        self.started('a')
        credential.write_text('refreshed-private-identity')
        b = self.start('b', 2)
        time.sleep(.2)
        self.assertFalse((self.root / 'b.start').exists())
        with (self.root / '.buckconfig.local').open('a') as output:
            output.write('instance_name = refreshed-instance\n')
        self.release('a')
        self.assertEqual(a.wait(timeout=10), 0)
        self.started('b')
        self.assertIn('instance_name = refreshed-instance', (self.root / '.buckconfig.local').read_text())
        self.assertEqual(credential.read_text(), 'refreshed-private-identity')
        self.assertEqual((self.root / 'kills').read_text(), 'kill\n')
        self.release('b')
        self.assertEqual(b.wait(timeout=10), 0)

    def test_limit_edit_and_symlink_rejection(self):
        value = '[buck2_re_client]\nengine_address = kept\n[other]\nvalue = kept\n'
        self.assertEqual(invocation.replace_limit(value, 3), '[buck2_re_client]\nengine_address = kept\nexecution_concurrency_limit = 3\n[other]\nvalue = kept\n')
        with self.assertRaises(ValueError):
            invocation.replace_limit('[buck2_re_client]\nexecution_concurrency_limit=1\nexecution_concurrency_limit=2\n', 3)
        target = self.root / 'untouched'
        target.write_text('original')
        local = self.root / '.buckconfig.local'
        local.unlink()
        local.symlink_to(target)
        process = self.start('refused', 1)
        self.assertEqual(process.wait(timeout=10), 2)
        self.assertEqual(target.read_text(), 'original')

    def test_links_back_into_the_project_never_survive_into_a_daemon(self):
        # A daemon that started while such a link existed watches the linked
        # source directories under the link's name and misses their changes.
        (self.root / 'src').mkdir()
        execroot = Path(self.temporary.name) / 'output-base/execroot'
        execroot.mkdir(parents=True)
        (execroot / 'src').symlink_to(self.root / 'src')
        outside = Path(self.temporary.name) / 'elsewhere'
        outside.mkdir()
        (self.root / 'scratch').symlink_to(outside)
        first = self.start('first', 2)
        self.started('first')
        self.release('first')
        self.assertEqual(first.wait(timeout=10), 0)
        self.assertFalse((self.root / 'kills').exists())
        (self.root / 'bazel-one').symlink_to(execroot)
        second = self.start('second', 2)
        self.started('second')
        self.assertFalse((self.root / 'bazel-one').is_symlink())
        self.assertEqual((self.root / 'kills').read_text(), 'kill\n')
        self.release('second')
        self.assertEqual(second.wait(timeout=10), 0)
        third = self.start('third', 2)
        self.started('third')
        self.release('third')
        self.assertEqual(third.wait(timeout=10), 0)
        self.assertEqual((self.root / 'kills').read_text(), 'kill\n')
        self.assertTrue((self.root / 'scratch').is_symlink())
        self.assertTrue((execroot / 'src').is_symlink())
        for target in (self.root / 'src', execroot):
            (self.root / 'alias').symlink_to(target)
            refused = self.start('refused', 2)
            self.assertEqual(refused.wait(timeout=10), 2)
            self.assertFalse((self.root / 'refused.start').exists())
            (self.root / 'alias').unlink()
        self.assertEqual((self.root / 'kills').read_text(), 'kill\n')

    def test_interrupt_waiter_does_not_cancel_active_client(self):
        active = self.start('active', 2)
        self.started('active')
        waiter = self.start('waiter', 1)
        time.sleep(.2)
        waiter.terminate()
        self.assertEqual(waiter.wait(timeout=3), 128 + signal.SIGTERM)
        self.assertIsNone(active.poll())
        self.assertFalse((self.root / 'waiter.start').exists())
        self.assertFalse((self.root / 'kills').exists())
        self.release('active')
        self.assertEqual(active.wait(timeout=10), 0)

    def test_interrupted_client_exit_zero_is_nonpass_and_preserves_outputs(self):
        process = self.start('handles-term', 2)
        self.started('handles-term')
        process.terminate()
        self.assertEqual(process.wait(timeout=3), 128 + signal.SIGTERM)
        self.assertEqual((self.root / 'handles-term.done').read_text(), 'completed-output')

    def test_failed_status_with_surviving_pid_rejects_transition_and_clean(self):
        (self.root / 'daemon.json').write_text('{"jobs": 2}')
        (self.root / 'mask-status').touch()
        metadata = self.root / 'daemon-metadata/kiln'
        (metadata / 'buckd.pid').write_text(str(os.getpid()))
        (metadata / 'buckd.info').write_text(json.dumps({'pid': os.getpid()}))
        original = (self.root / '.buckconfig.local').read_bytes()
        for exclusive in (False, True):
            with self.subTest(clean=exclusive):
                with self.assertRaisesRegex(ValueError, 'recorded daemon PID survives'):
                    with invocation.admission(self.root, self.root / 'buck2', 'kiln', 1, exclusive=exclusive):
                        self.fail('Admitted an unreachable surviving daemon')
                self.assertEqual((self.root / '.buckconfig.local').read_bytes(), original)
                self.assertFalse((self.root / '.buck2/invocations/kiln.json').exists())
                self.assertFalse((self.root / 'kills').exists())

    def test_dead_pid_allows_start_but_busy_lifecycle_rejects(self):
        import fcntl
        metadata = self.root / 'daemon-metadata/kiln'
        (metadata / 'buckd.pid').write_text('2147483647')
        (metadata / 'buckd.info').write_text('{"pid": 2147483647}')
        with (metadata / 'buckd.lifecycle').open('r') as lifecycle:
            fcntl.flock(lifecycle, fcntl.LOCK_EX)
            with self.assertRaisesRegex(ValueError, 'lifecycle is busy'):
                with invocation.admission(self.root, self.root / 'buck2', 'kiln', 1):
                    self.fail('Admitted a daemon during startup')
        process = self.start('dead-pid', 1)
        self.assertEqual(self.started('dead-pid')['jobs'], 1)
        self.release('dead-pid')
        self.assertEqual(process.wait(timeout=10), 0)

    def test_fifo_configuration_certificate_and_receipt_reject_without_holding_gate(self):
        import fcntl
        for kind in ('config', 'certificate', 'receipt'):
            with self.subTest(kind=kind):
                root = self.make_root(Path(self.temporary.name) / kind)
                local = root / '.buckconfig.local'
                if kind == 'config':
                    local.unlink()
                    special = local
                elif kind == 'certificate':
                    special = root / 'identity.pem'
                    with local.open('a') as output:
                        output.write('tls_client_cert = ' + str(special) + '\n')
                else:
                    state = root / '.buck2/invocations'
                    state.mkdir(parents=True, mode=0o700)
                    special = state / 'kiln.json'
                os.mkfifo(special)
                original = local.read_bytes() if kind != 'config' else None
                state = root / '.buck2/invocations'
                state.mkdir(parents=True, mode=0o700, exist_ok=True)
                descriptor = os.open(state / 'admission.lock', os.O_RDWR | os.O_CREAT, 0o600)
                with os.fdopen(descriptor, 'r+') as gate:
                    fcntl.flock(gate, fcntl.LOCK_EX)
                    cancelled = self.start('cancelled-fifo', 1, root)
                    time.sleep(.1)
                    cancelled.terminate()
                    self.assertEqual(cancelled.wait(timeout=3), 128 + signal.SIGTERM)
                process = self.start('fifo', 1, root)
                self.assertEqual(process.wait(timeout=3), 2)
                with (root / '.buck2/invocations/admission.lock').open('r') as gate:
                    fcntl.flock(gate, fcntl.LOCK_EX | fcntl.LOCK_NB)
                self.assertTrue(special.exists())
                self.assertFalse((root / 'fifo.start').exists())
                self.assertFalse((root / 'kills').exists())
                if original is not None:
                    self.assertEqual(local.read_bytes(), original)


if __name__ == '__main__':
    unittest.main()
