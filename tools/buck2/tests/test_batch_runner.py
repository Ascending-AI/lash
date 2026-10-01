import os
from pathlib import Path
import subprocess
import tempfile
import unittest

RUNNER = Path(__file__).resolve().parents[1] / 'test_batch_runner.sh'


class BatchRunnerTests(unittest.TestCase):
    """A batch member is `<n> <n NAME=value> <binary>`, as `test_batch.bzl` emits it."""

    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)

    def binary(self, name, body):
        path = self.root / name
        path.write_text('#!/usr/bin/env bash\n' + body)
        path.chmod(0o755)
        return str(path)

    def run_batch(self, *argv):
        env = {
            'PATH': '/usr/bin:/bin',
            'LASH_BATCH_JOBS': '2',
            'TEST_TMPDIR': str(self.root),
            'XML_OUTPUT_FILE': str(self.root / 'test.xml'),
        }
        return subprocess.run(
            ['/usr/bin/bash', str(RUNNER), *argv],
            cwd=self.root, env=env, capture_output=True, text=True, check=False,
        )

    def test_each_member_runs_with_its_own_environment(self):
        check = 'test "$CARGO_MANIFEST_DIR" = "$1" && test -z "${OTHER:-}"\n'
        first = self.binary('first', check.replace('$1', 'crates/first'))
        second = self.binary('second', check.replace('$1', 'crates/second') + 'test "$OUT_DIR" = "buck-out/out"\n')
        bare = self.binary('bare', 'test -z "${CARGO_MANIFEST_DIR:-}"\n')
        result = self.run_batch(
            '3',
            '1', 'CARGO_MANIFEST_DIR=crates/first', first,
            '2', 'CARGO_MANIFEST_DIR=crates/second', 'OUT_DIR=buck-out/out', second,
            '0', bare,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('PASS: 3 test binaries in batch', result.stdout)
        report = (self.root / 'test.xml').read_text()
        for name in (first, second, bare):
            self.assertIn(os.path.basename(name), report)

    def test_a_failing_member_fails_the_batch(self):
        passing = self.binary('passing', 'exit 0\n')
        failing = self.binary('failing', 'echo broken; exit 1\n')
        result = self.run_batch('2', '0', passing, '1', 'CARGO_PKG_NAME=x', failing)
        self.assertEqual(result.returncode, 1)
        self.assertIn('FAIL: 1 of 2 test binaries failed', result.stderr)
        self.assertIn('broken', result.stderr)

    def test_member_values_and_paths_are_never_word_split_or_expanded(self):
        directory = self.root / 'with space; $(touch pwned) *'
        directory.mkdir()
        value = 'two words; $(touch pwned) * `id` "quoted"'
        member = directory / 'member $x'
        member.write_text(
            '#!/usr/bin/env bash\n'
            'test "$VALUE" = \'' + value + '\' && test "$#" = 0\n'
        )
        member.chmod(0o755)
        result = self.run_batch('1', '1', 'VALUE=' + value, str(member))
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertIn('PASS: 1 test binaries in batch', result.stdout)
        self.assertFalse((self.root / 'pwned').exists())

    def test_a_multiline_value_stays_one_assignment(self):
        value = 'first line\nSECOND=leaked\n'
        member = self.binary(
            'member',
            'test "$VALUE" = $\'first line\\nSECOND=leaked\\n\' && test -z "${SECOND:-}" && test "$LAST" = kept\n',
        )
        result = self.run_batch('1', '2', 'VALUE=' + value, 'LAST=kept', member)
        self.assertEqual(result.returncode, 0, result.stderr)

    def test_a_binary_path_holding_an_equals_sign_still_runs(self):
        directory = self.root / 'mode=opt'
        directory.mkdir()
        member = directory / 'member'
        member.write_text('#!/usr/bin/env bash\ntest "$1" = "a=b" && echo ran-with-argument\nexit 3\n')
        member.chmod(0o755)
        result = self.run_batch('1', '1', 'NAME=value', str(member), 'a=b')
        self.assertEqual(result.returncode, 1)
        self.assertIn('ran-with-argument', result.stderr)

    def test_a_filter_matching_no_member_fails_the_batch(self):
        empty = (
            'echo; echo "running 0 tests"; echo\n'
            'echo "test result: ok. 0 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out; finished in 0.00s"\n'
        )
        first = self.binary('first', empty)
        second = self.binary('second', empty)
        result = self.run_batch('2', '0', first, '1', 'CARGO_PKG_NAME=x', second, 'no_such_case')
        self.assertEqual(result.returncode, 1, result.stdout)
        self.assertIn('no tests matched the batch arguments', result.stderr)

    def test_a_malformed_member_is_refused(self):
        lone = self.binary('lone', 'exit 0\n')
        result = self.run_batch('1', '2', 'A=1', lone)
        self.assertEqual(result.returncode, 1)
        self.assertIn('invalid environment count', result.stderr)


if __name__ == '__main__':
    unittest.main()
