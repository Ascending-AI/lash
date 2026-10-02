import json
import os
from pathlib import Path
import signal
import subprocess
import sys
import tempfile
import time
import unittest

BUCK2 = Path(__file__).resolve().parents[1]
ROOT = BUCK2.parents[1]
RUNNER = BUCK2 / 'postgres_action_runner.py'
PINNED_TREE = ROOT / '.buck2/native/postgres'
PINNED_NSS_WRAPPER = ROOT / '.buck2/native/nss_wrapper/libnss_wrapper.so'
SCHEMA = ROOT / 'crates/lash-postgres-store/schema.sql'

# A stand-in server tree: `initdb` makes the data directory, and `postgres`
# answers the startup and simple-query messages the runner sends, journalling
# what it was asked. The journal lives outside the cluster, which the runner
# deletes.
FAKE_INITDB = """#!/usr/bin/env python3
import json, os, sys
journal = os.environ['FAKE_POSTGRES_JOURNAL']
data = sys.argv[sys.argv.index('--pgdata') + 1]
with open(journal, 'a') as output:
    output.write(json.dumps({'initdb': data, 'preload': os.environ.get('LD_PRELOAD'), 'passwd': open(os.environ['NSS_WRAPPER_PASSWD']).read()}) + '\\n')
if os.environ.get('FAKE_INITDB_FAILS'):
    print('initdb: could not look up effective user ID', file=sys.stderr)
    sys.exit(1)
os.makedirs(data)
"""
FAKE_POSTGRES = """#!/usr/bin/env python3
import json, os, signal, socket, struct, sys
journal = os.environ['FAKE_POSTGRES_JOURNAL']
def note(**record):
    with open(journal, 'a') as output:
        output.write(json.dumps(record) + '\\n')
port = next(int(arg.split('=', 1)[1]) for arg in sys.argv if arg.startswith('port='))
def stop(number, frame):
    note(stopped=number)
    sys.exit(0)
signal.signal(signal.SIGINT, stop)
signal.signal(signal.SIGTERM, signal.SIG_IGN)
server = socket.socket()
server.setsockopt(socket.SOL_SOCKET, socket.SO_REUSEADDR, 1)
server.bind(('127.0.0.1', port))
server.listen(8)
note(pid=os.getpid(), arguments=sys.argv[1:])
def receive(connection, count):
    data = b''
    while len(data) < count:
        chunk = connection.recv(count - len(data))
        if not chunk:
            raise EOFError
        data += chunk
    return data
while True:
    connection, _ = server.accept()
    try:
        length = struct.unpack('!I', receive(connection, 4))[0]
        database = receive(connection, length - 4)[4:].split(b'\\0')[3].decode()
        connection.sendall(b'R' + struct.pack('!II', 8, 0) + b'Z' + struct.pack('!I', 5) + b'I')
        while True:
            kind, length = struct.unpack('!cI', receive(connection, 5))
            body = receive(connection, length - 4)
            if kind != b'Q':
                break
            note(database=database, sql=body[:-1].decode())
            connection.sendall(b'C' + struct.pack('!I', 7) + b'OK\\0' + b'Z' + struct.pack('!I', 5) + b'I')
    except EOFError:
        pass
    finally:
        connection.close()
"""


def alive(pid):
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    return True


class FakeServerTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory()
        self.addCleanup(self.directory.cleanup)
        self.work = Path(self.directory.name)
        self.tree = self.work / 'tree'
        (self.tree / 'bin').mkdir(parents=True)
        for name, source in (('initdb', FAKE_INITDB), ('postgres', FAKE_POSTGRES)):
            (self.tree / 'bin' / name).write_text(source)
            (self.tree / 'bin' / name).chmod(0o755)
        self.schema = self.work / 'schema.sql'
        self.schema.write_text('CREATE TABLE lash_fixture (id int);\n')
        self.journal = self.work / 'journal'
        # The runner's own temporary directory, so a leftover cluster is seen.
        self.clusters = self.work / 'clusters'
        self.clusters.mkdir()
        self.environment = {key: value for key, value in os.environ.items() if key != 'LASH_POSTGRES_DATABASE_URL'}
        self.environment.update(FAKE_POSTGRES_JOURNAL=str(self.journal), TMPDIR=str(self.clusters))

    def command(self, *command):
        return [sys.executable, str(RUNNER), str(self.tree), '/pinned/libnss_wrapper.so', str(self.schema), *command]

    def run_runner(self, *command, **environment):
        return subprocess.run(self.command(*command), env=self.environment | environment, capture_output=True, text=True, timeout=60)

    def records(self):
        return [json.loads(line) for line in self.journal.read_text().splitlines()]

    def assert_server_gone(self):
        records = self.records()
        pid = next(record['pid'] for record in records if 'pid' in record)
        deadline = time.monotonic() + 5
        while alive(pid) and time.monotonic() < deadline:
            time.sleep(.01)
        self.assertFalse(alive(pid), 'the server outlived the runner')
        self.assertEqual(list(self.clusters.iterdir()), [], 'the cluster outlived the runner')
        return records

    def test_the_command_runs_against_a_provisioned_private_server(self):
        show = 'import os; print(os.environ["LASH_POSTGRES_DATABASE_URL"])'
        result = self.run_runner(sys.executable, '-c', show)
        self.assertEqual(result.returncode, 0, result.stderr)
        records = self.assert_server_gone()
        initdb = records[0]
        self.assertEqual(initdb['preload'], '/pinned/libnss_wrapper.so')
        self.assertEqual(initdb['passwd'].split(':')[:3], ['lash', 'x', str(os.geteuid())])
        arguments = next(record['arguments'] for record in records if 'arguments' in record)
        port = next(argument.split('=')[1] for argument in arguments if argument.startswith('port='))
        self.assertIn('listen_addresses=127.0.0.1', arguments)
        self.assertIn('shared_preload_libraries=pg_stat_statements', arguments)
        self.assertEqual(result.stdout.split(), [f'postgres://lash:lash@127.0.0.1:{port}/lash'])
        statements = [(record['database'], record['sql']) for record in records if 'sql' in record]
        # Readiness is a query too; the provisioning follows it in order.
        self.assertEqual(statements[-2:], [('postgres', 'CREATE DATABASE "lash"'), ('lash', self.schema.read_text())])
        self.assertEqual(records[-1], {'stopped': signal.SIGINT})

    def test_a_failing_command_keeps_its_exit_code_and_leaves_nothing_behind(self):
        result = self.run_runner(sys.executable, '-c', 'raise SystemExit(3)')
        self.assertEqual(result.returncode, 3)
        self.assert_server_gone()
        killed = self.run_runner(sys.executable, '-c', 'import os, signal; os.kill(os.getpid(), signal.SIGKILL)')
        self.assertEqual(killed.returncode, 128 + signal.SIGKILL)

    def test_term_reaches_the_command_and_the_server_stops_after_it(self):
        ready = self.work / 'ready'
        script = (
            'import signal, sys, time\n'
            'from pathlib import Path\n'
            'signal.signal(signal.SIGTERM, lambda number, frame: sys.exit(7))\n'
            f'Path({str(ready)!r}).write_text("ready")\n'
            'time.sleep(60)\n'
        )
        runner = subprocess.Popen(self.command(sys.executable, '-c', script), env=self.environment, start_new_session=True)
        try:
            deadline = time.monotonic() + 30
            while not ready.exists():
                self.assertIsNone(runner.poll())
                self.assertLess(time.monotonic(), deadline)
                time.sleep(.01)
            runner.send_signal(signal.SIGTERM)
            self.assertEqual(runner.wait(timeout=30), 7)
        finally:
            try:
                os.killpg(runner.pid, signal.SIGKILL)
            except ProcessLookupError:
                pass
        self.assert_server_gone()

    def test_a_supplied_database_url_runs_the_command_unchanged(self):
        show = 'import os, sys; print(os.environ["LASH_POSTGRES_DATABASE_URL"], *sys.argv[1:])'
        result = self.run_runner(sys.executable, '-c', show, '--lash-libtest-args', LASH_POSTGRES_DATABASE_URL='postgres://external/db')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(result.stdout.split(), ['postgres://external/db', '--lash-libtest-args'])
        self.assertFalse(self.journal.exists(), 'a server was started beside the supplied one')

    def test_the_libtest_marker_is_dropped_only_on_request(self):
        show = 'import sys; print(*sys.argv[1:])'
        kept = self.run_runner(sys.executable, '-c', show, 'binary', '--lash-libtest-args', '--ignored')
        dropped = self.run_runner('--drop-libtest-marker', sys.executable, '-c', show, 'binary', '--lash-libtest-args', '--ignored')
        self.assertEqual(kept.stdout.split(), ['binary', '--lash-libtest-args', '--ignored'])
        self.assertEqual(dropped.stdout.split(), ['binary', '--ignored'])

    def test_a_cluster_that_cannot_be_initialized_fails_the_action(self):
        result = self.run_runner(sys.executable, '-c', 'print("ran")', FAKE_INITDB_FAILS='1')
        self.assertEqual(result.returncode, 70)
        self.assertNotIn('ran', result.stdout)
        self.assertIn('could not look up effective user ID', result.stderr)
        self.assertEqual(list(self.clusters.iterdir()), [])


@unittest.skipUnless(PINNED_TREE.is_dir() and PINNED_NSS_WRAPPER.is_file(), 'the pinned PostgreSQL is not bootstrapped in this checkout')
class PinnedServerTests(unittest.TestCase):
    def run_runner(self, clusters, script):
        environment = {key: value for key, value in os.environ.items() if key != 'LASH_POSTGRES_DATABASE_URL'}
        environment.update(TMPDIR=str(clusters), RUNNER_DIRECTORY=str(BUCK2))
        command = [sys.executable, str(RUNNER), str(PINNED_TREE), str(PINNED_NSS_WRAPPER), str(SCHEMA), sys.executable, '-c', script]
        return subprocess.run(command, env=environment, capture_output=True, text=True, timeout=120)

    def test_the_published_schema_and_statement_statistics_are_available(self):
        script = (
            'import os, sys\n'
            'sys.path.insert(0, os.environ["RUNNER_DIRECTORY"])\n'
            'import postgres_action_runner as runner\n'
            'port = int(os.environ["LASH_POSTGRES_DATABASE_URL"].rsplit(":", 1)[1].split("/")[0])\n'
            'runner.execute(port, "lash", "CREATE EXTENSION pg_stat_statements; SELECT count(*) FROM pg_stat_statements")\n'
            # Division by zero, and so an error, unless the schema's tables exist.
            'runner.execute(port, "lash", "SELECT 1 / count(*) FROM pg_tables WHERE tablename LIKE \'lash_%\'")\n'
            'runner.execute(port, "lash", "SELECT 1 / count(*) FROM pg_database WHERE datname = current_database() AND datlocprovider = \'i\'")\n'
        )
        with tempfile.TemporaryDirectory() as clusters:
            result = self.run_runner(clusters, script)
            self.assertEqual(result.returncode, 0, result.stderr)
            self.assertEqual(list(Path(clusters).iterdir()), [])
            failed = self.run_runner(clusters, 'raise SystemExit(5)')
            self.assertEqual(failed.returncode, 5)
            self.assertEqual(list(Path(clusters).iterdir()), [])
            # Every server process names its cluster directory.
            leftover = subprocess.run(['pgrep', '-f', clusters], capture_output=True, text=True)
            self.assertEqual(leftover.stdout, '')


if __name__ == '__main__':
    unittest.main()
