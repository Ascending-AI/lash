from pathlib import Path
import re
import unittest

ROOT = Path(__file__).resolve().parents[3]


def starlark_function(path, name):
    source = (ROOT / path).read_text()
    match = re.search(rf'^def {name}\(.*?(?=^def |\Z)', source, re.MULTILINE | re.DOTALL)
    namespace = {}
    exec(match.group(0), namespace)
    return namespace[name]


class TestInputTests(unittest.TestCase):
    """A test sees its declared files at their checkout paths, as under Cargo."""

    def test_every_declared_target_keeps_its_own_resource_name(self):
        resources = starlark_function('tools/buck2/lash_rust.bzl', '_resources')
        named = resources(
            ['tests/fixture.json', 'Cargo.toml'],
            [
                '//crates/lash-remote-protocol:rust_sources',
                '//crates/lash-trace:rust_sources',
                'native//:node',
                ':worker__bin',
                '//crates/lash-trace:rust_sources',
            ],
        )
        self.assertEqual(named['tests/fixture.json'], 'tests/fixture.json')
        self.assertEqual(named['Cargo.toml'], 'Cargo.toml')
        self.assertEqual(
            named['__lash_inputs__/crates/lash-remote-protocol/rust_sources'],
            '//crates/lash-remote-protocol:rust_sources',
        )
        self.assertEqual(
            named['__lash_inputs__/crates/lash-trace/rust_sources'],
            '//crates/lash-trace:rust_sources',
        )
        self.assertEqual(named['__lash_inputs__/native/node'], 'native//:node')
        self.assertEqual(named['__lash_inputs__/worker__bin'], ':worker__bin')
        self.assertEqual(len(named), 6)

    def test_no_test_action_runs_the_env_injecting_run_info(self):
        # A rust_test's RunInfo runs through the prelude's `test_env.json`,
        # which records absolute paths of the host that wrote it. The wrapper
        # and the batch take the test's own command and environment instead.
        wrapper = (ROOT / 'tools/buck2/test_rules.bzl').read_text()
        batch = (ROOT / 'tools/buck2/test_batch.bzl').read_text()
        self.assertNotIn('ctx.attrs.test[RunInfo]', wrapper)
        self.assertIn('"test": attrs.dep(providers = [DefaultInfo, ExternalRunnerTestInfo])', wrapper)
        self.assertNotIn('test[RunInfo]', batch)
        self.assertIn('"members": attrs.list(attrs.dep(providers = [ExternalRunnerTestInfo]))', batch)

    def test_generated_filegroups_link_their_checkout_files(self):
        for path in [ROOT / 'BUCK', ROOT / 'examples/BUCK', *ROOT.glob('*/*/BUCK')]:
            text = path.read_text()
            for block in re.findall(r'^filegroup\(\n.*?^\)', text, re.MULTILINE | re.DOTALL):
                with self.subTest(path=path.relative_to(ROOT).as_posix(), block=block.splitlines()[1]):
                    self.assertIn('    copy = False,\n', block)

    def test_runtime_files_are_exported_by_reference(self):
        text = (ROOT / 'BUCK').read_text()
        for name in ('Cargo.toml', 'Cargo.lock'):
            block = re.search(rf'^export_file\(\n    name = "{re.escape(name)}",\n.*?^\)', text, re.MULTILINE | re.DOTALL)
            self.assertIn('    mode = "reference",\n', block.group(0))


if __name__ == '__main__':
    unittest.main()
