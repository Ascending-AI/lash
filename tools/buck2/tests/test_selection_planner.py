from contextlib import redirect_stderr
import io
import json
from pathlib import Path
import tempfile
import unittest
import sys

HERE = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(HERE))
import driver
from test_selection import plan_test_command


class TestSelectionTests(unittest.TestCase):
    def fixture(self, root, targets):
        executable = root / 'buck2'
        (root / 'targets.json').write_text(json.dumps(targets))
        executable.write_text('#!/usr/bin/env python3\nimport json,sys\nfrom pathlib import Path\np=Path.cwd()\nassert sys.argv[3]=="uquery"\nwith (p/"query-argv.jsonl").open("a") as f:f.write(json.dumps(sys.argv[1:])+"\\n")\nprint((p/"targets.json").read_text())\n')
        executable.chmod(0o700)
        return executable

    def command(self, executable, root, args):
        options, remaining = driver.arguments(['test', *args])
        return driver.command(options, remaining, executable, root)

    def test_mixed_wildcard_and_explicit_manual_preserve_public_wrappers_once(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = self.fixture(root, {
                'root//pkg:public': {'labels': ['lash.timeout_seconds=300']},
                'root//pkg:public__rust_test': {'labels': ['lash.internal_test_binary']},
                'root//pkg:manual': {'labels': ['manual']},
                'root//pkg:ui': {'tags': ['manual']},
            })
            argv = self.command(executable, root, ['--target-platforms', '//tools/buck2:judged', '--modifier', '//modifiers/...', '//pkg/...', '//pkg:manual', '//pkg:public', '--test_arg=--ignored'])
            result = plan_test_command(argv, root)
            front, runner = result[:result.index('--')], result[result.index('--'):]
            self.assertEqual(front.count('root//pkg:public'), 1)
            self.assertNotIn('//pkg:public', front)
            self.assertIn('//pkg:manual', front)
            self.assertNotIn('root//pkg:ui', front)
            self.assertIn('root//pkg:public__rust_test', front)
            self.assertIn('--exclude=lash.internal_test_binary', front)
            self.assertIn('--always-exclude', front)
            self.assertIn('--test-arg=--ignored', runner)
            queries = [json.loads(line) for line in (root / 'query-argv.jsonl').read_text().splitlines()]
            self.assertEqual(len(queries), 1)
            self.assertEqual(queries[0][-1], '//pkg/...')

    def test_pattern_reports_the_manual_targets_it_skips_on_one_line(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = self.fixture(root, {
                'root//pkg:public': {'labels': []},
                'root//pkg:named': {'labels': ['manual']},
                'root//pkg:service': {'labels': ['cargo-service-gate', 'manual']},
                'root//pkg:ui': {'labels': ['cargo-trybuild', 'manual']},
                'root//pkg:ui__fv_0a1b2c3d': {'labels': ['feature-lane', 'manual']},
                'root//pkg:lib__fv_0a1b2c3d': {'labels': ['feature-lane', 'manual']},
            })
            output = io.StringIO()
            with redirect_stderr(output):
                result = plan_test_command(self.command(executable, root, ['//pkg/...', '//pkg:named']), root)
            front = result[:result.index('--')]
            self.assertEqual(front[-2:], ['root//pkg:public', '//pkg:named'])
            self.assertEqual(output.getvalue(), 'hermetic-build: patterns skip manual targets; name one to run it. Skipped: //pkg:service //pkg:ui 2 feature-lane variants\n')
            output = io.StringIO()
            with redirect_stderr(output):
                plan_test_command(self.command(executable, root, ['//pkg:public', '//pkg:named']), root)
            self.assertEqual(output.getvalue(), '')

    def test_relative_patterns_preserve_explicit_manual_targets(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = self.fixture(root, {
                'root//pkg:public': {'labels': []},
                'root//pkg:manual': {'labels': ['manual']},
            })
            result = plan_test_command(self.command(executable, root, ['pkg/...', 'pkg:manual', 'pkg:public']), root)
            self.assertIn('root//pkg:public', result)
            self.assertIn('pkg:manual', result)
            self.assertNotIn('//:dev_tests', result)
            self.assertNotIn('root//pkg:manual', result)
            self.assertNotIn('pkg:public', result)

    def test_explicit_manual_does_not_query_or_add_manual_exclusion(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = self.fixture(root, {})
            argv = self.command(executable, root, ['//pkg:manual', '--include=lash.internal_test_binary'])
            self.assertEqual(plan_test_command(argv, root), argv)
            self.assertFalse((root / 'query-argv.jsonl').exists())
            self.assertNotIn('--exclude=manual', argv)
            self.assertIn('--always-exclude', argv)

    def test_package_wildcards_filter_manual_and_reject_nonstatic_metadata(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            executable = self.fixture(root, {'root//pkg:manual': {'tags': ['manual']}})
            with self.assertRaisesRegex(ValueError, 'No non-manual targets'):
                plan_test_command(self.command(executable, root, ['//pkg:all']), root)
            query = json.loads((root / 'query-argv.jsonl').read_text().splitlines()[0])
            self.assertEqual(query[-1], '//pkg:')
            (root / 'targets.json').write_text(json.dumps({'root//pkg:maybe': {'labels': {'select': []}}}))
            with self.assertRaisesRegex(ValueError, 'static labels and tags'):
                plan_test_command(self.command(executable, root, ['//pkg:*']), root)


if __name__ == '__main__':
    unittest.main()
