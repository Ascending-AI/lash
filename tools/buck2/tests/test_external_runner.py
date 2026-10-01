from contextlib import nullcontext
import json
import os
from pathlib import Path
import subprocess
import sys
import tempfile
import threading
import time
from types import SimpleNamespace
import unittest
from unittest.mock import patch
import xml.etree.ElementTree as ET

TOOLS = Path(__file__).resolve().parents[1]
sys.path.insert(0, str(TOOLS))
import junit_xml
import test_runner as runner

# `//crates/lash-sim:cross_backend_store_differential__test` under
# `--nocapture --test-threads=1`, with its long lines cut. Six of the thirteen
# tests print before their result.
NOCAPTURE_LOG = '''
running 13 tests
test coalesced_batch_oracles::advancing_differential_clock_wall_clock_faces_agree ... ok
test coalesced_batch_oracles::coalesced_batches_match_literal_oracles_on_every_backend ... PASSED literal coalesced-batch oracles; fixtures=7; policies=2; cases=14
ok
test coalesced_batch_oracles::interrupted_admission_identity_stands_over_a_later_row ... PASSED interrupted-admission later-row literal oracle
ok
test cross_backend_store_differential_agrees ... PASS obligation_ledgers_and_recovery_lease: backends=3 ledger_steps=25
PASS session_delete_ledger: backends=3 steps=18
PASS generation_drains: backends=3 steps=15
RUNNING cross-backend store differential; cases=33
PASSED cross-backend store differential
ok
test differential_clock_wall_clock_faces_agree ... ok
test generated_catalog_covers_required_adversarial_shapes ... ok
test generated_surface::attachment_blob_store_differential_agrees ... SKIPPED attachment blob-store differential: LASH_REQUIRE_S3 is not set
ok
test generated_surface::generated_cross_backend_surface_differential_agrees ... cross-backend generated coverage is bounded: cases=4 first_seed=852
cross-backend generated case seed=852: covered_operation_kinds={"add_observer", "claim_wake"}
cross-backend generated case seed=853: covered_operation_kinds={"add_observer", "cancel_request"}
ok
test normalized_store_errors_compare_typedness_and_variant_not_prose ... ok
test residue::residue_digest_covers_a_planted_postgres_table ... ok
test residue::residue_digest_covers_a_planted_sqlite_table ... ok
test surface_sweep::usage_accounting_differential_agrees ... PASS usage_accounting: backends=3 steps=11
ok
test trait_surface_gate::store_trait_surface_is_fully_gated ... ok

test result: ok. 13 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 57.42s

'''

FAILING_LOG = '''
running 5 tests
test laws::passes_quietly ... ok
test laws::needs_a_service ... ignored, needs PostgreSQL
test laws::fails_after_output ... step 1 of 3
ok
step 2 of 3

thread 'laws::fails_after_output' panicked at src/lib.rs:3:5:
left != right
FAILED
test laws::passes_after_output ... warming up
ok
test laws::skipped ... ignored

failures:

failures:
    laws::fails_after_output

test result: FAILED. 2 passed; 1 failed; 2 ignored; 0 measured; 0 filtered out; finished in 0.10s

'''

# Several test threads: uncaptured output of running tests lands around the
# records libtest writes, with and without its own line ending.
INTERLEAVED_LOG = '''
running 4 tests
test a::first ... ok
child process says hello
test a::second ... a::third is half way
ok
a::fourth wrote no newlinetest a::third ... ok
test a::fourth ... a late write with no newlineok

test result: ok. 4 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.10s

'''


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
        self.options = SimpleNamespace(test_arg=[], timeout=None, no_test_cache=False, local_test_execution=False, test_output_dir=self.root / 'results', leases=self.root)

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


def verbatim(text):
    return runner.pb.ExternalRunnerSpecValue(verbatim=text)


def junit(*names, suite='//pkg:unit_test'):
    cases = ''.join(f'<testcase name="{name}" classname="{suite}"/>' for name in names)
    return f'<testsuites><testsuite name="{suite}">{cases}</testsuite></testsuites>'


class Buck2Outputs:
    """Buck2's declared-output layout: one directory per target and stage variant.

    The command is not part of the directory name. Each execution writes the
    report of the case it was asked for, and returns once `overlap` executions
    have written theirs.
    """

    def __init__(self, root, overlap=1):
        self.root = root
        self.barrier = threading.Barrier(overlap)
        self.guard = threading.Lock()
        self.active = self.peak = 0
        self.requests = []

    def Execute2(self, request):
        with self.guard:
            self.requests.append(request)
            self.active += 1
            self.peak = max(self.peak, self.active)
        try:
            executable = request.test_executable
            variant = executable.stage.testing.variant if executable.stage.testing.HasField('variant') else 'none'
            directory = self.root / str(executable.target.id) / variant / 'junit'
            directory.mkdir(parents=True, exist_ok=True)
            arguments = [arg.content.spec_value.verbatim for arg in executable.cmd if arg.content.spec_value.WhichOneof('value') == 'verbatim']
            case = arguments[-1]
            with self.guard:
                (directory / 'test.xml').write_text(junit(case))
            self.barrier.wait(timeout=10)
            time.sleep(0.05)
            result = runner.pb.ExecutionResult2(status=runner.pb.ExecutionStatus(finished=0))
            result.stdout.inline = f'test {case} ... ok\n'.encode()
            result.outputs.append(runner.pb.OutputEntry(declared_output=runner.pb.OutputName(name='junit'), output=runner.pb.Output(local_path=str(directory))))
            return runner.pb.ExecuteResponse2(result=result)
        finally:
            with self.guard:
                self.active -= 1

    def ReportTestResult(self, request):
        pass


class ConcurrentSelectionTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.spec = runner.pb.ExternalRunnerSpec(
            target=runner.pb.ConfiguredTarget(cell='root', package='pkg', target='unit_test', configuration='cfg', handle=runner.pb.ConfiguredTargetHandle(id=1)),
            command=[runner.pb.ExternalRunnerSpecValue(arg_handle=0), verbatim('--lash-libtest-args')],
        )

    def options(self, name, *test_args):
        return SimpleNamespace(test_arg=list(test_args), timeout=None, no_test_cache=True, local_test_execution=False, test_output_dir=self.root / name, leases=self.root)

    def invoke(self, client, selections):
        """Run one invocation per selection at once; return each one's report."""
        reports = {}

        def invocation(name, args):
            reports[name] = runner.run_test(client, self.spec, self.options(name, *args), {})

        threads = [threading.Thread(target=invocation, args=item) for item in selections.items()]
        for thread in threads:
            thread.start()
        for thread in threads:
            thread.join()
        return reports

    def cases(self, report):
        return [case.get('name') for case in ET.parse(report['outputs']['junit_xml']).getroot().iter('testcase')]

    def test_overlapping_invocations_with_different_filters_report_their_own_cases(self):
        client = Buck2Outputs(self.root / 'buck-out', overlap=2)
        reports = self.invoke(client, {'a': ['--exact', 'laws::first'], 'b': ['--exact', 'laws::second']})
        self.assertEqual({name: report['status'] for name, report in reports.items()}, {'a': 'PASS', 'b': 'PASS'})
        self.assertEqual(self.cases(reports['a']), ['laws::first'])
        self.assertEqual(self.cases(reports['b']), ['laws::second'])
        self.assertEqual(client.peak, 2)

    def test_a_shared_output_root_fails_both_invocations_instead_of_passing(self):
        client = Buck2Outputs(self.root / 'buck-out', overlap=2)
        # The layout before the variant existed, without the lease that follows from it.
        with patch.object(runner, 'selection_variant', return_value='shared'), patch.object(runner, 'output_lease', return_value=nullcontext()):
            reports = self.invoke(client, {'a': ['--exact', 'laws::first'], 'b': ['--exact', 'laws::second']})
        self.assertEqual(len({tuple(self.cases(report)) for report in reports.values()}), 1)
        statuses = sorted(report['status'] for report in reports.values())
        self.assertEqual(statuses, ['INFRA_FAILURE', 'PASS'])
        failed = next(report for report in reports.values() if report['status'] == 'INFRA_FAILURE')
        self.assertIn('Test report does not match its selection', failed['output_error'])

    def test_the_same_selection_runs_one_invocation_at_a_time(self):
        client = Buck2Outputs(self.root / 'buck-out')
        reports = self.invoke(client, {'a': ['--exact', 'laws::first'], 'b': ['--exact', 'laws::first']})
        self.assertEqual([report['status'] for report in reports.values()], ['PASS', 'PASS'])
        self.assertEqual(client.peak, 1)
        variants = {request.test_executable.stage.testing.variant for request in client.requests}
        self.assertEqual(len(variants), 1)

    def test_the_output_root_is_named_by_everything_the_runner_adds(self):
        base = (['--exact', 'case'], {'KEY': 'value'}, 300, False, False)
        names = {runner.selection_variant(*base)}
        for index, value in enumerate((['case'], {'KEY': 'other'}, 301, True, True)):
            changed = list(base)
            changed[index] = value
            names.add(runner.selection_variant(*changed))
        self.assertEqual(len(names), 6)
        self.assertEqual(runner.selection_variant(*base), runner.selection_variant(['--exact', 'case'], {'KEY': 'value'}, 300, False, False))
        self.assertRegex(runner.selection_variant(*base), r'^lash-[0-9a-f]{16}$')


class ReportGuardTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.xml = Path(temporary.name) / 'test.xml'

    def mismatch(self, reported, printed, args=(), complete=True):
        self.xml.write_text(junit(*reported))
        stdout = ''.join(f'test {name} ... ok\n' for name in printed)
        return runner.report_mismatch(self.xml, stdout, list(args), complete)

    def test_a_report_of_the_executed_selection_matches(self):
        self.assertIsNone(self.mismatch(['a::one', 'a::two'], ['a::one', 'a::two']))
        self.assertIsNone(self.mismatch(['a::one', 'a::two'], ['a::one', 'a::two'], ['a::']))
        self.assertIsNone(self.mismatch(['a::one'], ['a::one'], ['--exact', 'a::one', 'b::absent']))
        self.assertIsNone(self.mismatch(['a::one'], ['a::one'], ['--skip', 'two', '--test-threads', '1', 'a::']))

    def test_a_report_of_another_execution_never_matches(self):
        self.assertIn('disagree on 2 cases', self.mismatch(['b::other'], ['a::one'], ['a::one']))
        self.assertIn('disagree on 1 cases, e.g. a::two', self.mismatch(['a::one'], ['a::one', 'a::two']))

    def test_a_case_outside_the_filters_never_matches(self):
        self.assertIn('b::other, which is outside the selection a::', self.mismatch(['b::other'], [], ['a::']))
        self.assertIn('outside the selection a::one', self.mismatch(['a::one_more'], [], ['--exact', 'a::one']))
        self.assertIn('a::two, which the selection skips', self.mismatch(['a::two'], [], ['--skip=two']))
        self.assertIn('which the selection skips', self.mismatch(['a::two'], [], ['--skip', 'a::two', '--exact']))

    def test_unreadable_target_arguments_leave_only_filters_unchecked(self):
        self.assertIsNone(self.mismatch(['b::baked'], [], ['a::'], complete=False))
        self.assertIn('skips', self.mismatch(['b::baked'], [], ['--skip', 'baked'], complete=False))
        self.assertIn('disagree', self.mismatch(['b::baked'], ['a::one'], ['a::'], complete=False))

    def test_the_exit_record_is_not_a_case(self):
        self.xml.write_text('<testsuites><testsuite name="//pkg:t"><testcase name="//pkg:t" classname="//pkg:t"/></testsuite></testsuites>')
        self.assertIsNone(runner.report_mismatch(self.xml, 'PASS: 3 test binaries in batch\n', ['a::'], True))

    def test_target_arguments_join_the_selection_when_readable(self):
        command = [runner.pb.ExternalRunnerSpecValue(arg_handle=0), verbatim('before'), verbatim('--lash-libtest-args'), verbatim('--ignored')]
        self.assertEqual(runner.libtest_arguments(SimpleNamespace(command=command), ['a::']), (['--ignored', 'a::'], True))
        command.append(runner.pb.ExternalRunnerSpecValue(arg_handle=1))
        self.assertEqual(runner.libtest_arguments(SimpleNamespace(command=command), ['a::']), (['--ignored', 'a::'], False))


class LibtestRecordTests(unittest.TestCase):
    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)

    def write(self, text, code='0'):
        """Write one suite from a log; return the writer's exit code and its suite."""
        log = self.root / 'test.log'
        log.write_text(text)
        xml = self.root / 'test.xml'
        result = subprocess.run([sys.executable, str(TOOLS / 'junit_xml.py'), str(xml), '//pkg:t', code, '1.000', str(log)], capture_output=True, text=True)
        return result, ET.parse(xml).getroot()[0]

    def test_output_between_a_name_and_its_result_loses_no_case(self):
        outcomes, mismatch = junit_xml.libtest_cases(NOCAPTURE_LOG)
        self.assertIsNone(mismatch)
        self.assertEqual(len(outcomes), 13)
        self.assertEqual(set(outcomes.values()), {'ok'})
        for name in ('cross_backend_store_differential_agrees', 'generated_surface::attachment_blob_store_differential_agrees', 'surface_sweep::usage_accounting_differential_agrees'):
            self.assertIn(name, outcomes)
        result, suite = self.write(NOCAPTURE_LOG)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((suite.get('tests'), suite.get('failures'), suite.get('errors'), suite.get('skipped')), ('13', '0', '0', '0'))

    def test_a_failure_and_ignored_cases_keep_their_outcomes_around_output(self):
        outcomes, mismatch = junit_xml.libtest_cases(FAILING_LOG)
        self.assertIsNone(mismatch)
        self.assertEqual(outcomes, {
            'laws::passes_quietly': 'ok', 'laws::needs_a_service': 'ignored, needs PostgreSQL',
            'laws::fails_after_output': 'FAILED', 'laws::passes_after_output': 'ok', 'laws::skipped': 'ignored',
        })
        result, suite = self.write(FAILING_LOG, code='101')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((suite.get('tests'), suite.get('failures'), suite.get('errors'), suite.get('skipped')), ('5', '1', '0', '2'))
        failed = [case.get('name') for case in suite.iter('testcase') if case.find('failure') is not None]
        self.assertEqual(failed, ['laws::fails_after_output'])

    def test_records_split_by_other_threads_are_still_read(self):
        outcomes, mismatch = junit_xml.libtest_cases(INTERLEAVED_LOG)
        self.assertIsNone(mismatch)
        self.assertEqual(outcomes, {'a::first': 'ok', 'a::second': 'ok', 'a::third': 'ok', 'a::fourth': 'ok'})

    def test_timed_results_and_repeated_names_are_counted_per_record(self):
        log = (
            'running 1 test\ntest live::settles ... ignored\n\ntest result: ok. 0 passed; 0 failed; 1 ignored; 0 measured; 3 filtered out\n'
            'running 1 test\ntest live::settles ... ok <0.004s>\n\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 3 filtered out\n'
        )
        self.assertEqual(junit_xml.libtest_cases(log), ({'live::settles': 'ok'}, None))

    def test_output_that_names_no_test_has_nothing_to_account_for(self):
        terse = 'running 3 tests\n...\ntest result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
        self.assertEqual(junit_xml.libtest_cases(terse), ({}, None))
        self.assertEqual(junit_xml.libtest_cases('test b::bench ... bench: 12 ns/iter (+/- 1)\n\ntest result: ok. 0 passed; 0 failed; 0 ignored; 1 measured; 0 filtered out\n'), ({}, None))

    def test_a_report_that_does_not_add_up_to_the_summary_fails_the_test(self):
        log = (
            'running 3 tests\ntest a::kept ... ok\ntest a::lost ... output that swallowed its result\n'
            'test a::also_kept ... ok\n\ntest result: ok. 3 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
        )
        outcomes, mismatch = junit_xml.libtest_cases(log)
        self.assertEqual(sorted(outcomes), ['a::also_kept', 'a::kept'])
        self.assertEqual(mismatch, 'libtest reported 3 passed, 0 failed, 0 ignored; its output names 2 passed, 0 failed, 0 ignored')
        result, suite = self.write(log)
        self.assertEqual(result.returncode, 1)
        self.assertIn('FAIL: the report does not account for every test: //pkg:t: libtest reported 3 passed', result.stderr)
        self.assertEqual((suite.get('tests'), suite.get('errors')), ('3', '1'))
        error = next(case for case in suite.iter('testcase') if case.get('name') == '//pkg:t').find('error')
        self.assertEqual(error.get('message'), mismatch)

    def run_binary(self, log):
        """Run a passing binary that prints `log` under the launcher's report prefix."""
        source = self.root / 'binary.log'
        source.write_text(log)
        xml = self.root / 'out.xml'
        xml.unlink(missing_ok=True)
        result = subprocess.run(
            ['bash', str(TOOLS / 'test_xml_runner.sh'), '--selection-argv-count', '1', 'binary', '/usr/bin/cat', str(source)],
            capture_output=True, text=True, timeout=30,
            env=dict(os.environ, XML_OUTPUT_FILE=str(xml), TEST_BINARY='//pkg:t', TEST_TMPDIR=str(self.root)),
        )
        return result, ET.parse(xml).getroot()[0]

    def test_a_passing_binary_fails_when_its_report_does_not_add_up(self):
        result, suite = self.run_binary('running 2 tests\ntest a::kept ... ok\ntest a::lost ... swallowed\n\ntest result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n')
        self.assertEqual(result.returncode, 1)
        self.assertIn('does not account for every test', result.stderr)
        self.assertEqual(suite.get('errors'), '1')
        result, suite = self.run_binary(NOCAPTURE_LOG)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((suite.get('tests'), suite.get('errors')), ('13', '0'))

    def test_the_selection_guard_reads_split_records(self):
        xml = self.root / 'guard.xml'
        xml.write_text(junit('laws::fails_after_output', 'laws::passes_after_output', 'laws::passes_quietly', 'laws::needs_a_service', 'laws::skipped'))
        self.assertIsNone(runner.report_mismatch(xml, FAILING_LOG, ['laws::'], True))
        xml.write_text(junit('laws::passes_quietly', 'laws::needs_a_service', 'laws::skipped'))
        self.assertIn('disagree on 2 cases', runner.report_mismatch(xml, FAILING_LOG, ['laws::'], True))


if __name__ == '__main__':
    unittest.main()
