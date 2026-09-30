import json
from pathlib import Path
import sys
import tempfile
from types import SimpleNamespace
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
import test_runner as runner


class Client:
    def __init__(self, result):
        self.result = result
        self.requests = []
        self.verdicts = []

    def Execute2(self, request):
        self.requests.append(request)
        return runner.pb.ExecuteResponse2(result=self.result)

    def ReportTestResult(self, request):
        self.verdicts.append(request.result)


class RunnerTests(unittest.TestCase):
    def setUp(self):
        self.temporary = tempfile.TemporaryDirectory()
        self.addCleanup(self.temporary.cleanup)
        self.root = Path(self.temporary.name)
        self.junit = self.root / 'junit'
        self.junit.mkdir()
        (self.junit / 'test.xml').write_text('<testsuite><testcase name="actual-case"/></testsuite>')
        self.receipts = self.root / 'undeclared'
        (self.receipts / 'nested').mkdir(parents=True)
        (self.receipts / 'nested' / 'law.json').write_text('{"actual":"receipt"}')
        self.spec = runner.pb.ExternalRunnerSpec(
            target=runner.pb.ConfiguredTarget(cell='root', package='crates/sql', target='unit_test', handle=runner.pb.ConfiguredTargetHandle(id=1)),
            command=[runner.pb.ExternalRunnerSpecValue(arg_handle=0)],
            env={'KILN_ACTION_CPU_COUNT': runner.pb.ExternalRunnerSpecValue(verbatim='2')},
            labels=['lash.timeout_seconds=60'],
        )
        self.options = SimpleNamespace(test_arg=[], timeout=None, no_test_cache=False, local_test_execution=False, test_output_dir=self.root / 'results')

    def result(self, code=0):
        result = runner.pb.ExecutionResult2(status=runner.pb.ExecutionStatus(finished=code))
        result.stdout.inline = b'test actual-case ... ok\n'
        result.execution_details.execution_kind.remote_command.cache_hit = True
        for name, path in [('junit', self.junit), ('undeclared', self.receipts)]:
            result.outputs.append(runner.pb.OutputEntry(declared_output=runner.pb.OutputName(name=name), output=runner.pb.Output(local_path=str(path))))
        return result

    def test_cached_outputs_and_canonical_environment_are_preserved(self):
        client = Client(self.result())
        report = runner.run_test(client, self.spec, self.options, {'SERVICE_SETTING': 'fixture'})
        self.assertEqual(report['status'], 'PASS')
        self.assertTrue(report['cache'])
        self.assertEqual((Path(report['outputs']['undeclared']) / 'nested/law.json').read_text(), '{"actual":"receipt"}')
        request = client.requests[0]
        self.assertEqual(request.timeout.seconds, 75)
        self.assertEqual({entry.key: entry.value.content.spec_value.verbatim for entry in request.test_executable.env if entry.value.content.WhichOneof('value') == 'spec_value'}['KILN_ACTION_CPU_COUNT'], '2')
        self.assertEqual(client.verdicts[0].status, runner.pb.PASS)

    def test_failure_stays_failed_with_declared_outputs(self):
        client = Client(self.result(7))
        report = runner.run_test(client, self.spec, self.options, {})
        self.assertEqual((report['status'], report['exit_code']), ('FAIL', 7))
        self.assertIn('junit_xml', report['outputs'])
        self.assertEqual(client.verdicts[0].status, runner.pb.FAIL)

    def test_watchdog_timeout_is_native_timeout(self):
        (self.receipts / '_lash_runner').mkdir()
        (self.receipts / '_lash_runner/execution.json').write_text('{"timed_out":true}')
        client = Client(self.result(124))
        report = runner.run_test(client, self.spec, self.options, {})
        self.assertEqual(report['status'], 'TIMEOUT')
        self.assertEqual(client.verdicts[0].status, runner.pb.TIMEOUT)

    def test_passing_without_actual_xml_is_infrastructure_failure(self):
        (self.junit / 'test.xml').unlink()
        client = Client(self.result())
        report = runner.run_test(client, self.spec, self.options, {})
        self.assertEqual(report['status'], 'INFRA_FAILURE')
        self.assertEqual(client.verdicts[0].status, runner.pb.INFRA_FAILURE)

    def test_local_and_fresh_controls_apply_to_test_request(self):
        self.options.local_test_execution = True
        self.options.no_test_cache = True
        client = Client(self.result())
        runner.run_test(client, self.spec, self.options, {})
        self.assertEqual(client.requests[0].executor_override.name, 'local')
        self.assertTrue(client.requests[0].disable_test_execution_caching)

    def test_runtime_values_cannot_override_budget_or_output_contract(self):
        for key in ['KILN_ACTION_CPU_COUNT', 'XML_OUTPUT_FILE', 'TEST_UNDECLARED_OUTPUTS_DIR', 'LASH_TEST_TIMEOUT_SECONDS']:
            with self.assertRaises(ValueError):
                runner.parse_env([key + '=1'])

    def test_malformed_xml_is_infrastructure_failure(self):
        (self.junit / 'test.xml').write_text('<broken')
        client = Client(self.result())
        report = runner.run_test(client, self.spec, self.options, {})
        self.assertEqual(report['status'], 'INFRA_FAILURE')
        self.assertEqual(client.verdicts[0].status, runner.pb.INFRA_FAILURE)

    def test_failed_malformed_xml_preserves_original_verdict_and_every_artifact(self):
        (self.junit / 'test.xml').write_text('<truncated')
        client = Client(self.result(7))
        report = runner.run_test(client, self.spec, self.options, {})
        self.assertEqual((report['status'], report['exit_code']), ('FAIL', 7))
        self.assertEqual(client.verdicts[0].status, runner.pb.FAIL)
        self.assertIn('Invalid per-case XML', report['output_error'])
        self.assertEqual(Path(report['outputs']['junit_xml']).read_text(), '<truncated')
        self.assertIn('actual-case', Path(report['outputs']['log']).read_text())
        self.assertEqual((Path(report['outputs']['undeclared']) / 'nested/law.json').read_text(), '{"actual":"receipt"}')

    def test_handled_interruption_cannot_report_pass(self):
        (self.receipts / '_lash_runner').mkdir()
        (self.receipts / '_lash_runner/execution.json').write_text('{"interrupted_signal":15,"exit_code":0,"cleanup_complete":true}')
        client = Client(self.result(0))
        report = runner.run_test(client, self.spec, self.options, {})
        self.assertEqual(report['status'], 'FAIL')
        self.assertEqual(report['exit_code'], 0)
        self.assertEqual(client.verdicts[0].status, runner.pb.FAIL)
        self.assertIn('junit_xml', report['outputs'])

    def test_service_inputs_and_tag_force_local_uncached_without_affecting_pure_env(self):
        for environment, labels in [({'DATABASE_URL': 'postgres://fixture.invalid/db'}, []), ({'LASH_S3_ENDPOINT': 'http://fixture.invalid'}, []), ({}, ['cargo-service-gate'])]:
            self.spec.labels[:] = labels
            client = Client(self.result())
            runner.run_test(client, self.spec, self.options, environment)
            self.assertEqual(client.requests[0].executor_override.name, 'local')
            self.assertTrue(client.requests[0].disable_test_execution_caching)
        self.spec.labels[:] = ['postgres', 's3']
        client = Client(self.result())
        runner.run_test(client, self.spec, self.options, {'LASH_QUICK': '1'})
        self.assertFalse(client.requests[0].HasField('executor_override'))
        self.assertFalse(client.requests[0].disable_test_execution_caching)

    def test_cells_cannot_overwrite_each_others_outputs(self):
        self.spec.target.package = 'other/pkg'
        self.spec.target.target = 'test'
        first_result = self.result()
        first_result.stdout.inline = b'first log'
        first = runner.run_test(Client(first_result), self.spec, self.options, {})
        self.spec.target.cell = 'other'
        self.spec.target.package = 'pkg'
        second_result = self.result()
        second_result.stdout.inline = b'second log'
        second = runner.run_test(Client(second_result), self.spec, self.options, {})
        self.assertNotEqual(first['outputs']['log'], second['outputs']['log'])
        self.assertEqual(Path(first['outputs']['log']).read_text(), 'first log')
        self.assertEqual(Path(second['outputs']['log']).read_text(), 'second log')
        self.assertIn('/root/other/pkg/test/', first['outputs']['log'])
        self.assertIn('/other/pkg/test/', second['outputs']['log'])

    def test_per_target_timeout_and_explicit_override(self):
        self.spec.labels[:] = ['lash.timeout_seconds=900']
        client = Client(self.result())
        runner.run_test(client, self.spec, self.options, {})
        self.assertEqual(client.requests[0].timeout.seconds, 915)
        self.options.timeout = 1200
        runner.run_test(client, self.spec, self.options, {})
        self.assertEqual(client.requests[-1].timeout.seconds, 1215)

    def test_duplicate_configuration_cannot_overwrite_outputs(self):
        reports = runner.Reports(self.root / 'report.json', 'trace')
        reports.started(self.spec)
        self.spec.target.configuration = 'different'
        with self.assertRaisesRegex(ValueError, 'Duplicate test label'):
            reports.started(self.spec)

    def test_started_report_survives_without_completion(self):
        path = self.root / 'report.json'
        reports = runner.Reports(path, 'trace')
        reports.started(self.spec)
        report = json.loads(path.read_text())
        self.assertFalse(report['session_complete'])
        self.assertEqual(report['results']['root//crates/sql:unit_test']['status'], 'RUNNING')


if __name__ == '__main__':
    unittest.main()
