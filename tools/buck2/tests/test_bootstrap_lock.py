"""Real-process checks for preparation ownership across competing clients and checkouts."""
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
from unittest import mock

TOOLS = Path(__file__).resolve().parents[1]
BOOTSTRAP = TOOLS / 'bootstrap.py'
sys.path.insert(0, str(TOOLS))
import bootstrap_store

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
STORE_CLIENT = '''import os, sys, time
from pathlib import Path
sys.path.insert(0, os.environ['STORE_TOOLS'])
import bootstrap_store
checkout, log = Path(sys.argv[1]), Path(sys.argv[2])
def build(tree):
    with log.open('a') as out: out.write('build\\n')
    time.sleep(0.3)
    (tree / 'bin').mkdir(parents=True)
    (tree / 'bin/tool').write_text('pinned tool')
print(bootstrap_store.materialize(checkout, 'tool', {'sha256': 'pin'}, build, checkout / 'tool'))
'''
CARGO = '''#!/usr/bin/env python3
import os, sys
from pathlib import Path
assert sys.argv[1:4] == ['vendor', '--locked', '--versioned-dirs'], sys.argv
with open(os.environ['CARGO_LOG'], 'a') as out: out.write(sys.argv[-1] + '\\n')
crate = Path(sys.argv[-1]) / 'demo-1.0.0'
crate.mkdir(parents=True)
(crate / 'lib.rs').write_text('pub fn demo() {}')
'''
LOCKFILE = '''[[package]]
name = "lash-buck2-third-party"
version = "0.0.0"

[[package]]
name = "demo"
version = "1.0.0"
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
        shutil.copyfile(TOOLS / 'bootstrap_store.py', tools / 'bootstrap_store.py')
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
        process = subprocess.Popen([sys.executable, str(self.root / 'tools/buck2/bootstrap.py')], cwd=self.root, env=dict(os.environ, PROBE_TAG=tag, KILN_REAL_CARGO=sys.executable, LASH_BUCK2_STORE='off'), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
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


class BootstrapStoreTests(unittest.TestCase):
    IDENTITY = {'sha256': 'pin'}

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.configured = self.root / 'store'
        self.store = self.configured / bootstrap_store.LAYOUT
        self.builds = []
        self.use_store(str(self.configured))

    def use_store(self, value, ci=None):
        environment = mock.patch.dict(os.environ, {'LASH_BUCK2_STORE': value} if value is not None else {})
        environment.start()
        self.addCleanup(environment.stop)
        if value is None:
            os.environ.pop('LASH_BUCK2_STORE', None)
        os.environ.pop('CI', None)
        if ci:
            os.environ['CI'] = ci

    def build(self, tree):
        self.builds.append(tree)
        (tree / 'bin').mkdir(parents=True)
        (tree / 'bin/tool').write_text('pinned tool')
        (tree / 'bin/tool').chmod(0o755)
        (tree / 'bin/alias').symlink_to('tool')
        (tree / 'empty').mkdir()

    def checkout(self, name):
        path = self.root / name
        path.mkdir(exist_ok=True)
        return path

    def materialize(self, name, slot='tool', identity=None, symlink=False):
        checkout = self.checkout(name)
        shutil.rmtree(checkout / 'tool', ignore_errors=True)
        return bootstrap_store.materialize(checkout, slot, identity or self.IDENTITY, self.build, checkout / 'tool', symlink=symlink)

    def entry(self, slot='tool', identity=None):
        return self.store / 'entries' / bootstrap_store.entry_name(slot, identity or self.IDENTITY)

    def unseal(self, entry):
        for directory, _, _ in os.walk(entry):
            os.chmod(directory, 0o700)

    def test_checkout_gets_ordinary_read_only_files_identical_to_the_built_tree(self):
        self.assertTrue(self.materialize('first'))
        self.assertTrue(self.materialize('second'))
        self.assertEqual(len(self.builds), 1)
        for name in ('first', 'second'):
            tool = self.root / name / 'tool/bin/tool'
            self.assertFalse(tool.is_symlink())
            self.assertEqual(tool.read_text(), 'pinned tool')
            self.assertEqual(tool.stat().st_mode & 0o777, 0o555)
            self.assertEqual(os.readlink(tool.with_name('alias')), 'tool')
            self.assertTrue((self.root / name / 'tool/empty').is_dir())
            # The checkout owns its directories, so deleting it never needs the store.
            shutil.rmtree(self.root / name / 'tool')
        self.assertEqual((self.entry() / 'tree/bin/tool').read_text(), 'pinned tool')

    def test_concurrent_checkouts_fill_an_entry_once_and_all_succeed(self):
        log = self.root / 'builds'
        client = self.root / 'client.py'
        client.write_text(STORE_CLIENT)
        processes = [
            subprocess.Popen([sys.executable, str(client), str(self.checkout(f'fork-{index}')), str(log)], env=dict(os.environ, STORE_TOOLS=str(TOOLS)), stdout=subprocess.PIPE, stderr=subprocess.PIPE, text=True)
            for index in range(6)
        ]
        for process in processes:
            stdout, stderr = process.communicate(timeout=30)
            self.assertEqual((process.returncode, stdout.strip()), (0, 'True'), stderr)
        self.assertEqual(log.read_text(), 'build\n')
        for index in range(6):
            self.assertEqual((self.root / f'fork-{index}/tool/bin/tool').read_text(), 'pinned tool')
        self.assertEqual(list((self.store / 'tmp').iterdir()), [])
        self.assertEqual([path.name for path in (self.store / 'entries').iterdir()], [self.entry().name])

    def test_corrupt_entry_is_rebuilt_and_never_cloned(self):
        def truncate(entry):
            (entry / 'tree/bin/tool').chmod(0o755)
            (entry / 'tree/bin/tool').write_text('pinned')

        def rewrite(entry):
            (entry / 'tree/bin/tool').chmod(0o755)
            (entry / 'tree/bin/tool').write_text('poisoned!!!')

        damage = {
            'truncated file': truncate,
            'rewritten file': rewrite,
            'missing file': lambda entry: (entry / 'tree/bin/tool').unlink(),
            'added file': lambda entry: (entry / 'tree/bin/extra').write_text('stray'),
            'retargeted link': lambda entry: ((entry / 'tree/bin/alias').unlink(), (entry / 'tree/bin/alias').symlink_to('/bin/sh')),
            'missing manifest': lambda entry: (entry / 'manifest.json').unlink(),
            'unreadable manifest': lambda entry: (entry / 'manifest.json').write_text('{'),
        }
        self.assertTrue(self.materialize('first'))
        for count, (name, corrupt) in enumerate(damage.items(), start=2):
            with self.subTest(name):
                self.unseal(self.entry())
                corrupt(self.entry())
                self.assertTrue(self.materialize('second'))
                self.assertEqual(len(self.builds), count)
                self.assertEqual((self.root / 'second/tool/bin/tool').read_text(), 'pinned tool')
                self.assertEqual(sorted(path.name for path in (self.root / 'second/tool/bin').iterdir()), ['alias', 'tool'])
                self.assertEqual(os.readlink(self.root / 'second/tool/bin/alias'), 'tool')
        self.assertEqual(list((self.store / 'tmp').iterdir()), [])

    def test_partial_fill_is_never_visible_as_an_entry(self):
        def interrupted(tree):
            self.build(tree)
            raise KeyboardInterrupt

        checkout = self.checkout('first')
        with self.assertRaises(KeyboardInterrupt):
            bootstrap_store.materialize(checkout, 'tool', self.IDENTITY, interrupted, checkout / 'tool')
        self.assertFalse(self.entry().exists())
        self.assertFalse((checkout / 'tool').exists())
        # A killed builder leaves its staging directory behind; the next fill clears it.
        crashed = self.store / 'tmp' / (self.entry().name + '.crashed') / 'entry/tree'
        crashed.mkdir(parents=True)
        (crashed / 'half').write_text('partial')
        self.assertTrue(self.materialize('first'))
        self.assertEqual(sorted(path.name for path in (checkout / 'tool').iterdir()), ['bin', 'empty'])
        self.assertEqual(list((self.store / 'tmp').iterdir()), [])

    def test_verification_rehashes_content_that_kept_its_size_and_time(self):
        self.assertTrue(self.materialize('first'))
        tool = self.entry() / 'tree/bin/tool'
        before = tool.stat()
        tool.chmod(0o755)
        tool.write_text('pinned fool')
        tool.chmod(0o555)
        os.utime(tool, ns=(before.st_atime_ns, before.st_mtime_ns))
        self.assertEqual(bootstrap_store.verify(), 1)
        self.assertFalse(self.entry().exists())
        self.assertTrue(self.materialize('second'))
        self.assertEqual(len(self.builds), 2)
        self.assertEqual((self.root / 'second/tool/bin/tool').read_text(), 'pinned tool')
        self.assertEqual(bootstrap_store.verify(), 0)

    def test_hardlinks_to_one_stored_file_stay_bounded(self):
        with mock.patch.object(bootstrap_store, 'LINK_CAP', 3):
            for index in range(6):
                self.assertTrue(self.materialize(f'fork-{index}'))
        self.assertLessEqual((self.entry() / 'tree/bin/tool').stat().st_nlink, 3)
        for index in range(6):
            self.assertEqual((self.root / f'fork-{index}/tool/bin/tool').read_text(), 'pinned tool')
        self.assertEqual(len(self.builds), 1)

    def test_unusable_store_reports_fallback_without_building(self):
        blocker = self.root / 'file'
        blocker.write_text('not a directory')
        shared = self.root / 'shared'
        (shared / bootstrap_store.LAYOUT).mkdir(parents=True)
        (shared / bootstrap_store.LAYOUT).chmod(0o777)
        read_only = self.root / 'read-only'
        read_only.mkdir()
        read_only.chmod(0o500)
        self.addCleanup(read_only.chmod, 0o700)
        cases = {'off': ('off', None), 'CI default': (None, 'true'), 'not a directory': (str(blocker / 'store'), None), 'writable by others': (str(shared), None)}
        if os.getuid() != 0:
            cases['read-only parent'] = (str(read_only / 'store'), None)
        for name, (value, ci) in cases.items():
            with self.subTest(name):
                self.use_store(value, ci)
                self.assertIsNone(bootstrap_store.usable())
                self.assertFalse(self.materialize('first'))
                self.assertFalse((self.root / 'first/tool').exists())
        self.assertEqual(self.builds, [])
        self.use_store(str(self.configured), ci='true')
        self.assertTrue(self.materialize('first'), 'an explicit store applies under CI too')

    def test_prune_removes_only_entries_no_existing_checkout_references(self):
        old, new = {'sha256': 'old'}, {'sha256': 'new'}
        self.assertTrue(self.materialize('gone', identity=old))
        self.assertTrue(self.materialize('live', identity=new))
        self.assertTrue(self.materialize('live', slot='linked', symlink=True))
        self.assertTrue((self.root / 'live/tool').is_symlink())
        stale = self.store / 'tmp' / (self.entry(identity=old).name + '.crashed')
        stale.mkdir()
        self.assertEqual(bootstrap_store.prune(0), 0)
        self.assertTrue(self.entry(identity=old).exists(), 'its checkout still exists')
        self.assertFalse(stale.exists())
        shutil.rmtree(self.root / 'gone')
        self.assertEqual(bootstrap_store.prune(3600), 0)
        self.assertTrue(self.entry(identity=old).exists(), 'used within the retention period')
        with bootstrap_store._lock(self.store, self.entry(identity=old).name, exclusive=False):
            self.assertEqual(bootstrap_store.prune(0), 0)
            self.assertTrue(self.entry(identity=old).exists(), 'in use by another process')
        receipt = self.store / 'sync-receipts/old.json'
        receipt.write_text('{}')
        result = subprocess.run([sys.executable, str(BOOTSTRAP), '--prune-store', '--unused-for', '0'], capture_output=True, text=True)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('pruned 1 Buck2 bootstrap store entries', result.stdout)
        self.assertFalse(self.entry(identity=old).exists())
        self.assertFalse(receipt.exists())
        self.assertTrue(self.entry(identity=new).exists())
        self.assertTrue(self.entry('linked').exists())
        self.assertEqual((self.root / 'live/tool/bin/tool').read_text(), 'pinned tool')
        self.assertEqual(len(list((self.store / 'refs').iterdir())), 1)
        self.assertEqual(self.materialize('next', identity=old), True)
        self.assertEqual(len(self.builds), 4)


class VendorStoreTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name).resolve()
        self.cargo = self.root / 'cargo'
        self.cargo.write_text(CARGO)
        self.cargo.chmod(0o755)

    def checkout(self, name):
        root = self.root / name
        (root / 'tools/buck2').mkdir(parents=True)
        (root / 'third-party').mkdir()
        for script in ('bootstrap_vendor.py', 'bootstrap_store.py'):
            shutil.copyfile(TOOLS / script, root / 'tools/buck2' / script)
        (root / 'third-party/Cargo.toml').write_text('[package]\nname = "lash-buck2-third-party"\n')
        (root / 'third-party/Cargo.lock').write_text(LOCKFILE)
        return root

    def vendor(self, root, store, *arguments):
        environment = dict(os.environ, KILN_REAL_CARGO=str(self.cargo), CARGO_LOG=str(self.root / 'cargo.log'), LASH_BUCK2_STORE=store)
        environment.pop('CI', None)
        return subprocess.run([sys.executable, str(root / 'tools/buck2/bootstrap_vendor.py'), *arguments], cwd=root, env=environment, capture_output=True, text=True)

    def calls(self):
        return len((self.root / 'cargo.log').read_text().splitlines())

    def test_checkouts_link_one_vendored_tree_and_vendor_privately_without_a_store(self):
        store = str(self.root / 'store')
        first, second = self.checkout('first'), self.checkout('second')
        for root in (first, second):
            result = self.vendor(root, store)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(self.vendor(root, store, '--check').returncode, 0)
        self.assertEqual(self.calls(), 1)
        shared = Path(os.readlink(first / 'vendor'))
        self.assertEqual(os.readlink(second / 'vendor'), str(shared))
        self.assertTrue(shared.is_relative_to(self.root / 'store'))
        self.assertEqual((second / 'vendor/demo-1.0.0/lib.rs').read_text(), 'pub fn demo() {}')

        # A link to anything but this checkout's pinned tree is not current.
        (second / 'vendor').unlink()
        (second / 'vendor').symlink_to(self.root)
        self.assertEqual(self.vendor(second, store, '--check').returncode, 1)
        self.assertEqual(self.vendor(second, store).returncode, 0)
        self.assertEqual(os.readlink(second / 'vendor'), str(shared))

        # Without a store the checkout vendors into its own directory, as before.
        self.assertEqual(self.vendor(second, 'off', '--check').returncode, 1)
        result = self.vendor(second, 'off')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertFalse((second / 'vendor').is_symlink())
        self.assertEqual((second / 'vendor/demo-1.0.0/lib.rs').read_text(), 'pub fn demo() {}')
        self.assertEqual(self.calls(), 2)
        self.assertEqual(self.vendor(second, 'off', '--check').returncode, 0)
        self.assertEqual((shared / 'demo-1.0.0/lib.rs').read_text(), 'pub fn demo() {}')

        # A current private tree stays; only a stale one is replaced by the shared link.
        self.assertEqual(self.vendor(second, store).returncode, 0)
        self.assertFalse((second / 'vendor').is_symlink())
        (second / 'vendor/.lash-buck2-vendor.json').write_text('{}')
        self.assertEqual(self.vendor(second, store).returncode, 0)
        self.assertEqual(os.readlink(second / 'vendor'), str(shared))
        self.assertFalse((second / 'vendor.old').exists())
        self.assertEqual(self.calls(), 2)


if __name__ == '__main__':
    unittest.main()
