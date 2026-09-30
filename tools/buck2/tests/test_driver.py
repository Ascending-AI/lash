import importlib.util
from pathlib import Path
import tempfile
import json
import os
from types import SimpleNamespace
from unittest.mock import patch
import unittest
import sys

PATH = Path(__file__).resolve().parents[1] / 'driver.py'
sys.path.insert(0, str(PATH.parent))
spec = importlib.util.spec_from_file_location('driver', PATH)
driver = importlib.util.module_from_spec(spec)
spec.loader.exec_module(driver)

REMOTE_CONFIG = '''[buck2_re_client]
engine_address = grpcs://executor.fixture
cas_address = grpcs://cas.fixture
action_cache_address = grpcs://cache.fixture
instance_name = fixture
tls_ca_certs = .kiln/ca.pem
tls_client_cert = .kiln/client.pem
[kiln]
executor_runtime = fixture-runtime
'''


class DriverTests(unittest.TestCase):
    def command(self, args, inventory=None):
        options, remaining = driver.arguments(args)
        return driver.command(options, remaining, Path('/buck2'), Path('/repo'), inventory)

    def test_full_library_compile_and_native_cold_flags(self):
        inventory = {'packages': [{'targets': [{'label': '//lib:lib', 'build_label': '//lib:lib[static]'}]}], 'workspace_build_targets': ['//lib:lib[static]', '//bin:bin']}
        command = self.command(['build', '--jobs=2', '--no-remote-cache', '--write-to-cache-anyway', '--build-report=/report'], inventory)
        self.assertIn('//lib:lib[static]', command)
        self.assertIn('//bin:bin', command)
        self.assertIn('--no-remote-cache', command)
        self.assertIn('--write-to-cache-anyway', command)
        self.assertFalse(any('execution_concurrency_limit' in arg for arg in command))
        self.assertEqual(command[command.index('--materializations') + 1], 'none')

    def test_check_selects_metadata_for_libraries_and_tests(self):
        inventory = {
            'packages': [{'targets': [
                {'label': '//lib:lib', 'check_label': '//lib:lib[check]', 'build_label': '//lib:lib[static]'},
                {'label': '//lib:test', 'check_label': '//lib:test[check]', 'build_label': '//lib:test'},
            ]}],
            'workspace_check_targets': ['//lib:lib[check]', '//lib:test[check]'],
        }
        for labels in [[], ['//:workspace_compile'], ['//lib:lib', '//lib:test']]:
            with self.subTest(labels=labels):
                command = self.command(['check', *labels], inventory)
                self.assertIn('//lib:lib[check]', command)
                self.assertIn('//lib:test[check]', command)
                self.assertNotIn('//lib:lib[static]', command)

    def test_focused_clippy_and_doc_select_real_outputs_and_reject_unknown(self):
        inventory = {'packages': [{'targets': [{'label': '//lib:lib', 'clippy_label': '//lib:lib[clippy.txt]', 'doc_label': '//lib:lib[doc]'}]}]}
        self.assertIn('//lib:lib[clippy.txt]', self.command(['clippy', '//lib:lib'], inventory))
        self.assertIn('//lib:lib[doc]', self.command(['doc', '//lib:lib'], inventory))
        with self.assertRaisesRegex(ValueError, 'No generated clippy output'):
            self.command(['clippy', '//unknown:label'], inventory)

    def test_package_recursive_patterns_select_each_operation_output(self):
        inventory = {
            'packages': [
                {'targets': [
                    {'label': '//crates/sql:sql', 'build_label': '//crates/sql:sql[static]', 'check_label': '//crates/sql:sql[check]', 'clippy_label': '//crates/sql:sql[clippy.txt]'},
                    {'label': '//crates/sql:sql__unit_test', 'build_label': '//crates/sql:sql__unit_test', 'check_label': '//crates/sql:sql__unit_test[check]', 'clippy_label': '//crates/sql:sql__unit_test[clippy.txt]', 'tags': ['manual']},
                ]},
                {'targets': [{'label': '//crates/sql/nested:nested', 'check_label': '//crates/sql/nested:nested[check]'}]},
                {'targets': [{'label': '//crates/sqlite:sqlite', 'check_label': '//crates/sqlite:sqlite[check]'}]},
                {'targets': [{'cargo': 'cargo_only', 'label': None}]},
            ],
            'feature_lane_units': [{'label': '//crates/sql:sql__fv_1', 'check_label': '//crates/sql:sql__fv_1[check]'}],
        }
        check = self.command(['check', '//crates/sql/...'], inventory)
        self.assertEqual(check[-3:], ['//crates/sql:sql[check]', '//crates/sql:sql__unit_test[check]', '//crates/sql/nested:nested[check]'])
        self.assertNotIn('//crates/sql/...', check)
        self.assertEqual(self.command(['check', 'root//crates/sql:all'], inventory)[-2:], ['//crates/sql:sql[check]', '//crates/sql:sql__unit_test[check]'])
        self.assertEqual(len([arg for arg in self.command(['check', '//...'], inventory) if arg.endswith('[check]')]), 4)
        self.assertEqual(self.command(['clippy', '//crates/sql:'], inventory)[-2:], ['//crates/sql:sql[clippy.txt]', '//crates/sql:sql__unit_test[clippy.txt]'])
        self.assertEqual(self.command(['build', '//crates/sql/...'], inventory)[-2:], ['//crates/sql:sql[static]', '//crates/sql:sql__unit_test'])
        self.assertEqual(self.command(['build', '//tools/buck2/...'], inventory)[-1], '//tools/buck2/...')
        for operation in ('check', 'clippy', 'doc'):
            with self.subTest(operation=operation), self.assertRaisesRegex(ValueError, f'No generated {operation} targets match //tools/...'):
                self.command([operation, '//tools/...'], inventory)

    def test_test_and_analyze_accept_package_recursive_patterns(self):
        command = self.command(['test', '//crates/sql/...'])
        self.assertIn('//crates/sql/...', command[:command.index('--')])
        analyze = self.command(['--local', 'analyze', '//crates/sql/...', 'root//crates/core:core'])
        self.assertEqual(analyze[-1], 'deps(//crates/sql/...) + deps(//crates/core:core)')
        self.assertEqual(self.command(['--local', 'analyze'])[-1], 'deps(//:workspace_compile)')
        for bad in ('--show-output', '//a) + deps(//b', '//crates/sql:sql[check]'):
            with self.subTest(bad=bad), self.assertRaisesRegex(ValueError, 'analyze accepts only target labels and patterns'):
                self.command(['--local', 'analyze', bad])

    def test_test_controls_do_not_disable_compile_cache(self):
        command = self.command(['test', '--local-test-execution', '--no-test-cache', '--test_arg=--ignored', '--test_timeout=1200', '//store:unit_test'])
        front, runner = command[:command.index('--')], command[command.index('--') + 1:]
        self.assertNotIn('--local-only', front)
        self.assertNotIn('--no-remote-cache', front)
        self.assertNotIn('--materializations', front)
        self.assertIn('--local-test-execution', runner)
        self.assertIn('--no-test-cache', runner)
        self.assertIn('--test-arg=--ignored', runner)
        self.assertEqual(runner[runner.index('--timeout') + 1], '1200')

    def test_service_runtime_values_force_local_uncached_test_only(self):
        for name in ['DATABASE_URL', 'LASH_POSTGRES_DATABASE_URL', 'LASH_S3_ENDPOINT', 'LASH_REQUIRE_S3', 'PGHOST']:
            command = self.command(['test', '--test_env=' + name + '=fixture', '//pkg:test'])
            front, runner = command[:command.index('--')], command[command.index('--') + 1:]
            self.assertIn('kiln.execution_mode=remote', front)
            self.assertNotIn('--local-only', front)
            self.assertIn('--local-test-execution', runner)
            self.assertIn('--no-test-cache', runner)
        command = self.command(['test', '--test_env=LASH_QUICK=1', '//pkg:test'])
        self.assertNotIn('--no-test-cache', command)
        self.assertNotIn('--local-test-execution', command)

    def test_run_preserves_invocation_and_workspace_environment(self):
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            (root / '.buckconfig.local').write_text(REMOTE_CONFIG)
            current = Path.cwd()
            try:
                with patch.object(driver, 'ROOT', root), patch.object(driver.subprocess, 'run'), patch.object(driver, 'run_command', return_value=0) as call, patch.dict(os.environ, {'LASH_BUILD_WORKING_DIRECTORY': '/original/subdirectory'}):
                    self.assertEqual(driver.main(['run', '//pkg:bin']), 0)
                    environment = call.call_args.kwargs['env']
                    self.assertEqual(environment['BUILD_WORKING_DIRECTORY'], '/original/subdirectory')
                    self.assertEqual(environment['BUILD_WORKSPACE_DIRECTORY'], str(root))
                    self.assertNotIn('LASH_BUILD_WORKING_DIRECTORY', environment)
            finally:
                os.chdir(current)

    def test_service_environment_is_resolved_in_caller(self):
        self.assertEqual(driver.resolve_environment(['SERVICE', 'OTHER=value'], {'SERVICE': 'secret'}), {'SERVICE': 'secret', 'OTHER': 'value'})
        with self.assertRaises(ValueError):
            driver.resolve_environment(['MISSING'], {})
        with self.assertRaises(ValueError):
            driver.resolve_environment(['KILN_ACTION_CPU_COUNT=100'], {})

    def test_run_uses_native_final_materialization(self):
        command = self.command(['run', '//bin:bin', '--', 'argument'])
        self.assertNotIn('--materializations', command)
        self.assertEqual(command[-3:], ['//bin:bin', '--', 'argument'])

    def test_final_output_download_uses_stock_buck_materialization_value(self):
        inventory = {'packages': [{'targets': [{'label': '//lib:lib', 'build_label': '//lib:lib[static]', 'doc_label': '//lib:lib[doc]'}]}]}
        for args in [['build', '//lib:lib', '--materializations=final'], ['doc', '//lib:lib']]:
            with self.subTest(args=args):
                command = self.command(args, inventory)
                self.assertEqual(command[command.index('--materializations') + 1], 'all')

    def test_profiles_select_their_distinct_declared_target_platforms(self):
        for profile in ['judged', 'optimized']:
            command = self.command(['run', '--config=' + profile, '//bin:bin'])
            self.assertEqual(command[command.index('--target-platforms') + 1], '//tools/buck2:' + profile)
            self.assertEqual(command[-1], '//bin:bin')
            self.assertEqual('kiln.rust_profile=optimized' in command, profile == 'optimized')

    def test_runtime_secret_file_is_private_and_removed_after_failure(self):
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            (root / '.buckconfig.local').write_text(REMOTE_CONFIG)
            (root / '.buck2').mkdir()
            seen = []
            def safe_directory(path):
                path.mkdir(mode=0o700)
                return path
            def call(root, jobs, isolation, argv, **kwargs):
                self.assertNotIn('private-value', ' '.join(argv))
                path = Path(argv[argv.index('--test-env-file') + 1])
                self.assertEqual(path.stat().st_mode & 0o077, 0)
                self.assertEqual(json.loads(path.read_text()), {'SERVICE': 'private-value'})
                seen.append(path)
                return 32
            current = Path.cwd()
            try:
                with patch.object(driver, 'ROOT', root), patch.object(driver.subprocess, 'run'), patch.object(driver, 'run_command', side_effect=call), patch.dict(sys.modules, {'runner_bootstrap': SimpleNamespace(ensure_runtime=lambda: None, safe_directory=safe_directory)}):
                    self.assertEqual(driver.main(['test', '--test_env=SERVICE=private-value', '//pkg:test']), 32)
            finally:
                os.chdir(current)
            self.assertEqual(len(seen), 1)
            self.assertFalse(seen[0].exists())

    def test_local_analyze_uses_supported_query_flags(self):
        command = self.command(['--local', 'analyze'])
        self.assertIn('cquery', command)
        self.assertIn('--show-providers', command)
        self.assertNotIn('--local-only', command)
        self.assertNotIn('--num-threads', command)

    def test_remote_configuration_requires_pool_metadata_but_local_remains_available(self):
        with tempfile.TemporaryDirectory() as work:
            root = Path(work)
            local = root / '.buckconfig.local'
            local.write_text(
                '[buck2_re_client]\n'
                'execution_concurrency_limit = 1\n'
            )
            with self.assertRaisesRegex(
                ValueError,
                r'Shared executor configuration is incomplete .*engine_address.*executor_runtime.*kiln fork lash <name>',
            ):
                driver.validate_remote_configuration(local)

            current = Path.cwd()
            try:
                with patch.object(driver, 'ROOT', root), patch.object(
                    driver.subprocess, 'run'
                ), patch.object(driver, 'run_command', return_value=0) as run:
                    self.assertEqual(driver.main(['--local', 'analyze']), 0)
                    self.assertTrue(run.called)
            finally:
                os.chdir(current)

            local.write_text(REMOTE_CONFIG)
            driver.validate_remote_configuration(local)

    def test_missing_remote_configuration_names_a_real_recovery(self):
        with tempfile.TemporaryDirectory() as work:
            missing = Path(work) / '.buckconfig.local'
            with self.assertRaisesRegex(
                ValueError, r'kiln fork lash <name>.*pass --local'
            ) as error:
                driver.validate_remote_configuration(missing)
            self.assertNotIn('refresh', str(error.exception))


if __name__ == '__main__':
    unittest.main()
