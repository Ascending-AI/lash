from contextlib import nullcontext
import json
import os
from pathlib import Path
import re
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
import shard_weights
import test_runner as runner
import test_shard

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


CHILD_SUMMARY = 'test result: {}. {} passed; {} failed; 0 ignored; 0 measured; 709 filtered out; finished in 0.00s\n\n'


def child_block(name, outcome='ok'):
    """What a child running one test of a libtest binary writes to the parent's stdout."""
    failed = outcome == 'FAILED'
    details = f'failures:\n\n---- {name} stdout ----\nchild panicked\n\n\nfailures:\n    {name}\n\n' if failed else ''
    return f'\nrunning 1 test\ntest {name} ... {outcome}\n\n{details}' + CHILD_SUMMARY.format('FAILED' if failed else 'ok', int(not failed), int(failed))


def outer_summary(passed, failed=0):
    return f'\ntest result: {"FAILED" if failed else "ok"}. {passed} passed; {failed} failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.00s\n\n'


# Several test threads, as a kernel test binary prints:
# each test re-runs itself, or a helper, in children that write to the
# parent's stdout. One child fails as its parent expects, one prints a name
# the binary did not run, and another test's record lands inside a child's block.
NESTED_LOG = (
    '\nrunning 4 tests\n'
    'test guard::first ... ok\n'
    + child_block('guard::reruns_itself') + child_block('guard::reruns_itself')
    + 'test guard::reruns_itself ... ok\n'
    + child_block('guard::expects_a_failing_child', 'FAILED')
    + 'test guard::expects_a_failing_child ... ok\n'
    + '\nrunning 1 test\ntest guard::uses_a_helper ... ok\ntest helper::child_only ... ok\n\n' + CHILD_SUMMARY.format('ok', 1, 0)
    + outer_summary(4)
)
NESTED_CASES = {'guard::first': 'ok', 'guard::reruns_itself': 'ok', 'guard::expects_a_failing_child': 'ok', 'guard::uses_a_helper': 'ok'}

# One test thread, the shape that failed `integration__test`: libtest names the
# test before it runs, so the children's blocks sit between each name and its
# result. The last test fails although its child, running the same name, passed.
NESTED_MID_LINE_LOG = (
    '\nrunning 4 tests\n'
    'test guard::first ... ok\n'
    'test guard::reruns_itself ... ' + child_block('guard::reruns_itself') + child_block('guard::reruns_itself') + 'ok\n'
    'test guard::expects_a_failing_child ... ' + child_block('guard::expects_a_failing_child', 'FAILED') + 'ok\n'
    'test guard::fails_after_its_child ... ' + child_block('guard::fails_after_its_child') + 'FAILED\n'
    '\nfailures:\n\n---- guard::fails_after_its_child stdout ----\nparent panicked\n\n\nfailures:\n    guard::fails_after_its_child\n'
    + outer_summary(3, 1)
)


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

    def test_non_libtest_commands_reject_no_forwarded_libtest_flags(self):
        # A strict Python command, like ui_fixtures_runner.py, must receive its
        # declared arguments and native options, never the scheduled timing flags.
        self.spec.test_type = 'custom'
        self.spec.ClearField('command')
        self.spec.command.extend(verbatim(arg) for arg in [
            sys.executable, '-c',
            'import argparse; p = argparse.ArgumentParser(); '
            'p.add_argument("--manifest"); p.add_argument("--native"); '
            'p.add_argument("selector"); '
            'assert vars(p.parse_args()) == '
            '{"manifest": "fixtures.json", "native": "kept", "selector": "case"}',
            '--manifest', 'fixtures.json',
        ])
        for flags in [
            ['-Z', 'unstable-options', '--report-time'],
            ['--exact', '--nocapture', '--ignored', '--include-ignored', '--list',
             '--test-threads', '1', '--format=pretty', '--skip', 'omitted',
             '--color', 'never', '--logfile=log', '--shuffle-seed', '7',
             '--quiet', '--show-output', '--ensure-time', '--shuffle',
             '--exclude-should-panic', '--bench', '--test', '-Zunstable-options'],
        ]:
            with self.subTest(flags=flags):
                self.options.test_arg = [*flags, '--native', 'kept', 'case']
                client = Client(self.result())
                runner.run_test(client, self.spec, self.options, {})
                command = [arg.content.spec_value.verbatim for arg in client.requests[0].test_executable.cmd]
                result = subprocess.run(command, capture_output=True, text=True)
                self.assertEqual(result.returncode, 0, result.stderr)

    def test_libtest_commands_keep_timing_and_selection_arguments(self):
        args = ['-Z', 'unstable-options', '--report-time', '--exact', 'actual-case']
        self.options.test_arg = args
        # Native rust tests, wrapped/sharded/service-prefixed rust tests, and
        # batches all run libtest even though the latter two have custom type.
        for test_type, command, environment in [
            ('rust', [verbatim('binary')], {}),
            ('custom', [verbatim('launcher'), verbatim('binary'),
                        verbatim('--lash-libtest-args'), verbatim('--ignored')], {}),
            ('custom', [verbatim('batch-launcher'), verbatim('1'), verbatim('0'),
                        verbatim('binary')], {'LASH_BATCH_JOBS': verbatim('1')}),
        ]:
            with self.subTest(test_type=test_type, command=command):
                self.spec.test_type = test_type
                self.spec.ClearField('command')
                self.spec.command.extend(command)
                self.spec.env.clear()
                for key, val in environment.items():
                    self.spec.env[key].CopyFrom(val)
                client = Client(self.result())
                report = runner.run_test(client, self.spec, self.options, {})
                self.assertEqual(report['status'], 'PASS', report)
                forwarded = [arg.content.spec_value.verbatim for arg in client.requests[0].test_executable.cmd]
                self.assertEqual(forwarded, [arg.verbatim for arg in command] + args)

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

    def test_a_reported_mode_suffix_still_matches_its_exact_selection(self):
        # libtest prints `#[should_panic]`, compile_fail and no_run cases with
        # a ` - <mode>` suffix; the selector names the test plainly.
        self.assertIsNone(self.mismatch(
            ['a::panics - should panic'], ['a::panics - should panic'], ['--exact', 'a::panics'],
        ))
        self.assertIsNone(self.mismatch(
            ['src/x.rs - case (line 3) - compile fail'], ['src/x.rs - case (line 3) - compile fail'],
            ['--exact', 'src/x.rs - case (line 3)'],
        ))
        self.assertIsNone(self.mismatch(
            ['src/x.rs - case (line 3) - compile'], ['src/x.rs - case (line 3) - compile'],
            ['--exact', 'src/x.rs - case (line 3)'],
        ))

    def test_a_different_case_with_the_same_suffix_is_still_outside(self):
        self.assertIn('a::other - should panic, which is outside the selection a::panics', self.mismatch(
            ['a::other - should panic'], ['a::other - should panic'], ['--exact', 'a::panics'],
        ))
        self.assertIn('which the selection skips', self.mismatch(
            ['a::two - should panic'], ['a::two - should panic'], ['--skip', 'a::two', '--exact'],
        ))

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

    def suite_counts(self, suite):
        return tuple(suite.get(key) for key in ('tests', 'failures', 'errors', 'skipped'))

    def test_nested_child_blocks_add_no_case_and_no_mismatch(self):
        self.assertEqual(junit_xml.libtest_cases(NESTED_LOG), (NESTED_CASES, None))
        result, suite = self.write(NESTED_LOG)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.suite_counts(suite), ('4', '0', '0', '0'))
        self.assertEqual([case.get('name') for case in suite.iter('testcase')], list(NESTED_CASES))
        result, suite = self.run_binary(NESTED_LOG)
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.suite_counts(suite), ('4', '0', '0', '0'))

    def test_a_nested_block_between_a_name_and_its_result_keeps_the_outer_result(self):
        outcomes, mismatch = junit_xml.libtest_cases(NESTED_MID_LINE_LOG)
        self.assertIsNone(mismatch)
        self.assertEqual(outcomes, {
            'guard::first': 'ok', 'guard::reruns_itself': 'ok',
            'guard::expects_a_failing_child': 'ok', 'guard::fails_after_its_child': 'FAILED',
        })
        result, suite = self.write(NESTED_MID_LINE_LOG, code='101')
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual(self.suite_counts(suite), ('4', '1', '0', '0'))
        failure = next(case for case in suite.iter('testcase') if case.get('name') == 'guard::fails_after_its_child').find('failure')
        self.assertEqual(failure.text, 'parent panicked')

    def test_a_child_case_named_like_an_outer_case_never_stands_in_for_it(self):
        # The child's outcome is not the binary's: the parent decides, and prints last.
        for child, outer in (('FAILED', 'ok'), ('ok', 'FAILED')):
            log = '\nrunning 2 tests\ntest a::other ... ok\n' + child_block('a::same', child) + f'test a::same ... {outer}\n' + outer_summary(1 + (outer == 'ok'), int(outer == 'FAILED'))
            self.assertEqual(junit_xml.libtest_cases(log), ({'a::other': 'ok', 'a::same': outer}, None))
        # Another test's record inside the child's block is still the binary's own.
        log = '\nrunning 2 tests\n\nrunning 1 test\ntest a::same ... ok\ntest a::other ... ok\n\n' + CHILD_SUMMARY.format('ok', 1, 0) + 'test a::same ... ok\n' + outer_summary(2)
        self.assertEqual(junit_xml.libtest_cases(log), ({'a::other': 'ok', 'a::same': 'ok'}, None))
        # A child's name printed before its parent's whole record, on one line.
        log = '\nrunning 2 tests\n\nrunning 1 test\ntest a::same ... test a::other ... ok\nok\n\n' + CHILD_SUMMARY.format('ok', 1, 0) + 'test a::same ... ok\n' + outer_summary(2)
        self.assertEqual(junit_xml.libtest_cases(log), ({'a::other': 'ok', 'a::same': 'ok'}, None))

    def test_one_selected_test_and_its_child_announce_the_same_count(self):
        for threads in ('test a::same ... ' + child_block('a::same') + 'ok\n', child_block('a::same') + 'test a::same ... ok\n'):
            log = '\nrunning 1 test\n' + threads + '\ntest result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 709 filtered out; finished in 0.10s\n\n'
            self.assertEqual(junit_xml.libtest_cases(log), ({'a::same': 'ok'}, None))

    def test_a_child_that_dies_without_a_summary_does_not_take_its_parents(self):
        dead = '\nrunning 1 test\n'
        log = '\nrunning 2 tests\ntest a::probes ... ' + dead + 'ok\ntest a::after ... ok\n' + outer_summary(2)
        self.assertEqual(junit_xml.libtest_cases(log), ({'a::probes': 'ok', 'a::after': 'ok'}, None))
        log = '\nrunning 2 tests\n' + dead + 'test a::probes ... ok\n' + child_block('a::after') + 'test a::after ... ok\n' + outer_summary(2)
        self.assertEqual(junit_xml.libtest_cases(log), ({'a::probes': 'ok', 'a::after': 'ok'}, None))
        log = '\nrunning 1 test\n' + dead + 'test a::probes ... ok\n' + outer_summary(1)
        self.assertEqual(junit_xml.libtest_cases(log), ({'a::probes': 'ok'}, None))
        log = '\nrunning 1 test\n' + dead + 'test a::probes ... swallowed\n' + outer_summary(1)
        self.assertEqual(junit_xml.libtest_cases(log)[1], 'libtest reported 1 passed, 0 failed, 0 ignored; its output names 0 passed, 0 failed, 0 ignored')

    def test_a_nested_block_does_not_hide_an_outer_summary_mismatch(self):
        # The binary says five passed and names four; the children's five records do not make up the difference.
        log = NESTED_LOG.replace(outer_summary(4), outer_summary(5))
        outcomes, mismatch = junit_xml.libtest_cases(log)
        self.assertEqual(outcomes, NESTED_CASES)
        self.assertEqual(mismatch, 'libtest reported 5 passed, 0 failed, 0 ignored; its output names 4 passed, 0 failed, 0 ignored')
        result, suite = self.write(log)
        self.assertEqual(result.returncode, 1)
        self.assertIn('does not account for every test', result.stderr)
        self.assertEqual(self.suite_counts(suite), ('5', '0', '1', '0'))
        result, suite = self.run_binary(log)
        self.assertEqual(result.returncode, 1)
        # A failure the binary's summary does not report is a mismatch too.
        log = NESTED_LOG.replace('test guard::first ... ok', 'test guard::first ... FAILED')
        self.assertEqual(junit_xml.libtest_cases(log)[1], 'libtest reported 4 passed, 0 failed, 0 ignored; its output names 3 passed, 1 failed, 0 ignored')

    def test_a_missing_outer_case_fails_whatever_its_children_printed(self):
        expected = 'libtest reported 4 passed, 0 failed, 0 ignored; its output names 3 passed, 0 failed, 0 ignored'
        # Lost with children that printed the same name, and with one that printed a foreign name.
        for lost in ('test guard::reruns_itself ... ok\n', 'test guard::uses_a_helper ... ok\n', 'test guard::first ... ok\n'):
            # The binary's own record of a name is the last one printed.
            before, after = NESTED_LOG.rsplit(lost, 1)
            log = before + ('' if 'helper' in lost else lost.replace(' ... ok', ' ... output that swallowed its result')) + after
            outcomes, mismatch = junit_xml.libtest_cases(log)
            self.assertEqual(mismatch, expected, lost)
            self.assertEqual(len(outcomes), 3, lost)
            result, suite = self.write(log)
            self.assertEqual(result.returncode, 1, lost)
            self.assertEqual(suite.get('errors'), '1', lost)
        log = NESTED_MID_LINE_LOG.replace(child_block('guard::reruns_itself') + 'ok\n', child_block('guard::reruns_itself') + 'no result\n')
        self.assertEqual(junit_xml.libtest_cases(log)[1], 'libtest reported 3 passed, 1 failed, 0 ignored; its output names 2 passed, 1 failed, 0 ignored')

    def test_a_childs_lost_record_is_not_the_binarys_to_account_for(self):
        log = NESTED_LOG.replace('test guard::reruns_itself ... ok\n', 'test guard::reruns_itself ... output that swallowed its result\n', 1)
        self.assertEqual(junit_xml.libtest_cases(log), (NESTED_CASES, None))

    def test_the_selection_guard_counts_only_the_binarys_own_cases(self):
        xml = self.root / 'guard.xml'
        xml.write_text(junit(*NESTED_CASES))
        for log in (NESTED_LOG, NESTED_LOG.replace(outer_summary(4), outer_summary(5))):
            self.assertIsNone(runner.report_mismatch(xml, log, ['guard::'], True))
        # A child's case in the report is foreign, and a missing own case is missing.
        xml.write_text(junit(*NESTED_CASES, 'helper::child_only'))
        self.assertIn('disagree on 1 cases, e.g. helper::child_only', runner.report_mismatch(xml, NESTED_LOG, [], True))
        xml.write_text(junit(*list(NESTED_CASES)[:3]))
        self.assertIn('disagree on 1 cases, e.g. guard::uses_a_helper', runner.report_mismatch(xml, NESTED_LOG, ['guard::'], True))

    def test_the_selection_guard_reads_split_records(self):
        xml = self.root / 'guard.xml'
        xml.write_text(junit('laws::fails_after_output', 'laws::passes_after_output', 'laws::passes_quietly', 'laws::needs_a_service', 'laws::skipped'))
        self.assertIsNone(runner.report_mismatch(xml, FAILING_LOG, ['laws::'], True))
        xml.write_text(junit('laws::passes_quietly', 'laws::needs_a_service', 'laws::skipped'))
        self.assertIn('disagree on 2 cases', runner.report_mismatch(xml, FAILING_LOG, ['laws::'], True))


# The accounting above reads a member's stdout as one libtest stream. A test
# that re-runs its binary must keep the child's stream out of it:
# `.output()`/`.wait_with_output()` capture both ends, and a `.spawn()` or
# `.status()` must first point `.stdout()` and `.stderr()` at a pipe, a file
# or null. An inherited descriptor writes the child's `running N tests`
# block into the parent's stream, where an unlucky interleave loses one of
# the parent's own records — the false mismatch the fixtures above replay.
# The scan below flags each self-exec libtest launch left on inherited stdio.

ROOT = TOOLS.parents[1]

COMMAND_NEW = re.compile(r'(?<!\w)Command::new\s*\(')
LET_BINDING = re.compile(r'let\s+(?:mut\s+)?(\w+)\s*=')
LIBTEST_SELECTION = re.compile(
    r'--(?:exact|nocapture|ignored|include-ignored|test-threads|list|format|report-time)\b'
    r'|"[^"\n]*::[^"\n]*"'
)
TEST_ATTRIBUTE = re.compile(r'#\[(?:\w+::)*test\b|#\[cfg\(test\)\]')
RAW_STRING = re.compile(r'(?:br|r)(#*)"')
CHAR_LITERAL = re.compile(r"b?'(?:\\u\{[0-9A-Fa-f]+\}|\\.|[^\\'])'")


def rust_structure(text):
    """`text` with comment, string and char bodies blanked; positions kept.

    The scan reasons about `;`-separated statements, so punctuation inside
    literals and comments must not shape them.
    """
    masked = list(text)
    limit = len(text)
    position = 0
    while position < limit:
        if text.startswith('//', position):
            end = text.find('\n', position)
            end = limit if end < 0 else end
        elif text.startswith('/*', position):
            depth, end = 0, position
            while end < limit:
                if text.startswith('/*', end):
                    depth, end = depth + 1, end + 2
                elif text.startswith('*/', end):
                    depth, end = depth - 1, end + 2
                    if not depth:
                        break
                else:
                    end += 1
        elif text[position] == '"' or text.startswith('b"', position):
            end = position + (text[position] == 'b') + 1
            while end < limit:
                if text[end] == '\\':
                    end += 2
                elif text[end] == '"':
                    end += 1
                    break
                else:
                    end += 1
            end = min(end, limit)
        elif match := RAW_STRING.match(text, position):
            closer = '"' + match.group(1)
            end = text.find(closer, match.end())
            end = limit if end < 0 else end + len(closer)
        elif match := CHAR_LITERAL.match(text, position):
            end = match.end()
        else:
            position += 1
            continue
        for index in range(position, end):
            if text[index] != '\n':
                masked[index] = ' '
        position = end
    return ''.join(masked)


def call_arguments(masked, found):
    """The argument list of the call whose `(` `found` ends at, from `masked`."""
    depth, index = 1, found.end()
    while index < len(masked) and depth:
        depth += (masked[index] == '(') - (masked[index] == ')')
        index += 1
    return masked[found.end():index - 1]


def test_source(path, source):
    """The file feeds a test target, so a self-exec in it runs libtest."""
    name = path.name
    return (
        'tests' in path.parts
        or name == 'tests.rs'
        or name.startswith('test_')
        or name.endswith(('_test.rs', '_tests.rs'))
        or bool(TEST_ATTRIBUTE.search(source))
    )


def uncaptured_self_execs(source):
    """Line numbers of self-exec launches that run libtest on inherited stdio.

    A launch counts as libtest when it passes libtest flags or a `a::b` case
    name, or passes no arguments at all (the whole suite runs). A self-exec
    without either is some other mode of the binary — a worker role or a
    sibling path — and this scan leaves it alone.
    """
    code = rust_structure(source)
    bound = set()
    pending = {}
    findings = []
    start, line = 0, 1

    def launched(state):
        if (state['libtest'] or not state['args']) and not (state['stdout'] and state['stderr']):
            findings.append(state['line'])

    def configured(state, statement):
        state['args'] |= bool(re.search(r'\.args?\s*\(', statement))
        for stream in ('stdout', 'stderr'):
            for found in re.finditer(rf'\.{stream}\s*\(', statement):
                state[stream] = 'inherit' not in call_arguments(statement, found)

    for semicolon in re.finditer(';', code):
        statement, raw = code[start:semicolon.end()], source[start:semicolon.end()]
        start = semicolon.end()
        for name in LET_BINDING.findall(statement):
            pending.pop(name, None)
            if 'current_exe' in statement and 'Command::new' not in statement:
                bound.add(name)
            else:
                bound.discard(name)
        commands = list(COMMAND_NEW.finditer(statement))
        self_exec = bool(commands) and (
            'current_exe' in statement
            or any(
                'current_exe' in call_arguments(statement, found)
                or (head := re.fullmatch(r'&?\s*(\w+)', call_arguments(statement, found)))
                and head.group(1) in bound
                for found in commands
            )
        )
        if self_exec:
            state = {'args': False, 'stdout': False, 'stderr': False,
                     'line': line + statement[:commands[0].start()].count('\n'),
                     'libtest': bool(LIBTEST_SELECTION.search(raw))}
            configured(state, statement)
            if not re.search(r'\.(?:output|wait_with_output)\s*\(', statement):
                if re.search(r'\.(?:status|spawn)\s*\(', statement):
                    launched(state)
                elif name := LET_BINDING.search(statement):
                    pending[name.group(1)] = state
        for name, state in list(pending.items()):
            if not re.search(rf'(?<![\w.]){name}\s*\.', statement):
                continue
            configured(state, statement)
            state['libtest'] |= bool(LIBTEST_SELECTION.search(raw))
            if re.search(rf'(?<![\w.]){name}\s*\.\s*(?:output|wait_with_output)\s*\(', statement):
                del pending[name]
            elif re.search(rf'(?<![\w.]){name}\s*\.\s*(?:status|spawn)\s*\(', statement):
                launched(state)
                del pending[name]
        line += raw.count('\n')
    return findings


class ChildStdioTests(unittest.TestCase):
    """No test target launches its own binary on inherited stdout or stderr."""

    def test_no_self_exec_leaks_libtest_output_into_the_parent(self):
        findings = []
        for base in ('crates', 'examples', 'fuzz'):
            for path in sorted((ROOT / base).rglob('*.rs')):
                source = path.read_text()
                if 'current_exe' not in source or not test_source(path, source):
                    continue
                findings += [
                    f'{path.relative_to(ROOT)}:{line}'
                    for line in uncaptured_self_execs(source)
                ]
        self.assertEqual(findings, [])


# A libtest double: `--list`, positional filters, `--skip`, `--exact` and
# `--ignored` behave as libtest's do. It appends the cases it ran to $SHARD_CALLS.
FAKE_LIBTEST = '''#!/usr/bin/env python3
import json, os, sys
cases = json.loads(os.environ["SHARD_CASES"])
args = sys.argv[1:]
if args == ["--list", "--format", "terse"]:
    print("".join(name + ": test\\n" for name in cases), end="")
    raise SystemExit(0)
filters, skips, position = [], [], 0
while position < len(args):
    argument = args[position]
    if argument in ("--skip", "--format", "--color", "--test-threads", "-Z"):
        position += 1
        if argument == "--skip":
            skips.append(args[position])
    elif argument.startswith("--skip="):
        skips.append(argument[len("--skip="):])
    elif not argument.startswith("-"):
        filters.append(argument)
    position += 1
exact = "--exact" in args
if args.count("--exact") > 1:
    raise SystemExit("Option 'exact' given more than once")
match = lambda name, value: name == value if exact else value in name
ran = [
    name for name, ignored in cases.items()
    if (not filters or any(match(name, value) for value in filters))
    and not any(match(name, value) for value in skips)
    and ignored == ("--ignored" in args)
]
with open(os.environ["SHARD_CALLS"], "a", encoding="utf-8") as output:
    output.write(json.dumps({"args": args, "ran": ran}) + "\\n")
'''


class ShardTests(unittest.TestCase):
    # The always-replay parts' names contain one another, and the plain
    # matrix's name is a prefix of them all.
    CASES = {
        'matrix': False,
        'matrix_always_replay': False,
        'matrix_always_replay_part_0': False,
        'matrix_always_replay_part_0_of_4': False,
        'other::law': False,
        'other::law_needs_service': True,
    }

    def setUp(self):
        temporary = tempfile.TemporaryDirectory()
        self.addCleanup(temporary.cleanup)
        self.root = Path(temporary.name)
        self.binary = self.root / 'fake-libtest'
        self.binary.write_text(FAKE_LIBTEST)
        self.binary.chmod(0o755)
        self.calls = self.root / 'calls.jsonl'
        self.env = dict(os.environ, SHARD_CALLS=str(self.calls), SHARD_CASES=json.dumps(self.CASES))

    def shard(self, count, index, *arguments, weights=()):
        return subprocess.run(
            [sys.executable, str(TOOLS / 'test_shard.py'), str(count), str(index), *weights, str(self.binary), *arguments],
            env=self.env, capture_output=True, text=True, timeout=30,
        )

    def shards(self, count, *arguments, weights=()):
        """Run every shard; return the cases each ran."""
        self.calls.unlink(missing_ok=True)
        for index in range(count):
            result = self.shard(count, index, *arguments, weights=weights)
            self.assertEqual(result.returncode, 0, result.stderr)
        return [json.loads(line)['ran'] for line in self.calls.read_text().splitlines()]

    def unsharded(self, *arguments):
        self.calls.unlink(missing_ok=True)
        subprocess.run([str(self.binary), *arguments], env=self.env, check=True, timeout=30)
        return json.loads(self.calls.read_text())['ran']

    def table(self, rows):
        path = self.root / 'weights.json'
        path.write_text(json.dumps(rows))
        return path

    def test_names_that_contain_one_another_run_once_each_on_their_own_shards(self):
        ran = self.shards(4)
        arguments = json.loads(self.calls.read_text().splitlines()[0])['args']
        self.assertEqual(sorted(name for shard in ran for name in shard), self.unsharded())
        self.assertEqual(ran, [
            ['matrix', 'other::law'],
            ['matrix_always_replay'],
            ['matrix_always_replay_part_0'],
            ['matrix_always_replay_part_0_of_4'],
        ])
        self.assertEqual(arguments.count('--exact'), 1)
        self.assertEqual(arguments.count('--skip'), 4)

    def test_the_callers_filters_select_what_they_select_unsharded(self):
        for arguments in (
            ['matrix_always'],
            ['matrix', '--exact'],
            ['matrix', 'other', '--skip', 'part_0'],
            ['--skip=matrix'],
            ['--skip', 'matrix_always_replay', '--exact'],
            ['--ignored'],
            ['--ignored', 'other::law'],
            ['-Z', 'unstable-options', '--report-time', 'law'],
            ['no_such_case'],
        ):
            with self.subTest(arguments=arguments):
                ran = self.shards(3, *arguments)
                kept = json.loads(self.calls.read_text().splitlines()[0])['args']
                self.assertEqual(sorted(name for shard in ran for name in shard), sorted(self.unsharded(*arguments)))
                # Only the selection is rewritten; libtest's other arguments pass through.
                passed = [argument for argument in arguments if argument in ('-Z', 'unstable-options', '--report-time', '--ignored')]
                self.assertEqual(kept[:len(passed)], passed)
        # A case keeps its shard whatever the filter.
        self.assertEqual(self.shards(3, 'matrix_always'), [[name for name in shard if 'matrix_always' in name] for shard in self.shards(3)])

    def test_weighted_cases_are_balanced_longest_first(self):
        weights = {'a': 100, 'b': 90, 'c': 50, 'd': 40, 'e': 10, 'f': 10}
        assignments = test_shard.shard_assignments(list(reversed(weights)), 3, weights)
        self.assertEqual(assignments, {'a': 0, 'b': 1, 'c': 2, 'd': 2, 'e': 1, 'f': 2})
        loads = [sum(weights[name] for name in weights if assignments[name] == index) for index in range(3)]
        self.assertEqual(loads, [100, 100, 100])
        # The three heavy matrices no longer share a shard, and no shard is empty.
        row = {'matrix': 90000, 'matrix_always_replay': 80000, 'matrix_always_replay_part_0': 85000,
               'matrix_always_replay_part_0_of_4': 70000, 'other::law': 500, 'other::law_needs_service': 1}
        ran = self.shards(4, weights=['--weights', str(self.table({'//pkg:t': row})), '//pkg:t__fv_0873c7ac'])
        self.assertEqual(ran, [['matrix'], ['matrix_always_replay_part_0'], ['matrix_always_replay'], ['matrix_always_replay_part_0_of_4', 'other::law']])

    def test_weights_named_as_reported_still_balance_the_listed_cases(self):
        # The table's keys carry libtest's ` - <mode>` suffix; the listing does not.
        row = {'matrix - should panic': 90000, 'matrix_always_replay - compile fail': 80000,
               'matrix_always_replay_part_0': 85000, 'matrix_always_replay_part_0_of_4 - compile': 70000,
               'other::law': 500, 'other::law_needs_service': 1}
        ran = self.shards(4, weights=['--weights', str(self.table({'//pkg:t': row})), '//pkg:t'])
        self.assertEqual(ran, [['matrix'], ['matrix_always_replay_part_0'], ['matrix_always_replay'], ['matrix_always_replay_part_0_of_4', 'other::law']])
        weights = {'a - should panic': 50, 'b': 20}
        self.assertEqual(test_shard.shard_assignments(['a', 'b', 'c'], 2, weights), {'a': 0, 'b': 1, 'c': 1})

    def test_unweighted_cases_go_round_robin_in_name_order(self):
        names = ['d', 'b', 'a', 'c', 'e']
        self.assertEqual(test_shard.shard_assignments(names, 2), {'a': 0, 'b': 1, 'c': 0, 'd': 1, 'e': 0})
        self.assertEqual(test_shard.shard_assignments(names, 2), test_shard.shard_assignments(sorted(names), 2, {}))
        # Beside weighted cases the round starts at the lightest shard.
        self.assertEqual(test_shard.shard_assignments(names, 3, {'e': 50, 'd': 20}), {'e': 0, 'd': 1, 'a': 2, 'b': 1, 'c': 0})

    def test_a_missing_or_stale_table_costs_balance_and_never_coverage(self):
        plain = self.shards(4)
        # No row for the test: the round-robin split.
        unmeasured = ['--weights', str(self.table({'//pkg:other': {'matrix': 5}})), '//pkg:t']
        self.assertEqual(self.shards(4, weights=unmeasured), plain)
        # A stale row names a case the binary dropped and misses the others.
        stale = ['--weights', str(self.table({'//pkg:t': {'removed::case': 9000, 'other::law': 400}})), '//pkg:t']
        ran = self.shards(4, weights=stale)
        self.assertEqual(sorted(name for shard in ran for name in shard), self.unsharded())
        self.assertEqual(ran, [
            ['matrix_always_replay_part_0_of_4', 'other::law'],
            ['matrix'],
            ['matrix_always_replay'],
            ['matrix_always_replay_part_0'],
        ])
        # A table that was declared but cannot be read stops the shard before it runs anything.
        self.calls.unlink(missing_ok=True)
        for weights in (
            ['--weights', str(self.root / 'absent.json'), '//pkg:t'],
            ['--weights', str(self.table({'//pkg:t': {'matrix': '5'}})), '//pkg:t'],
            ['--weights', str(self.table({'//pkg:t': ['matrix']})), '//pkg:t'],
        ):
            self.assertNotEqual(self.shard(4, 0, weights=weights).returncode, 0)
        self.assertFalse(self.calls.exists())

    def test_plan_balances_reported_modes_as_listed_cases(self):
        row = {'heavy - should panic': 10000, 'middle - compile fail': 9000, 'light': 8000}
        self.assertEqual(shard_weights.plan({'//pkg:t': row}, '//pkg:t', 2), [
            'shard 1/2: 1 cases, 10000 ms',
            'shard 2/2: 2 cases, 17000 ms',
        ])

    def test_reported_case_times_refresh_the_table(self):
        def report(name, runs):
            results = {}
            for label, cases in runs.items():
                log = self.root / f'{name}-{len(results)}.log'
                log.write_text(
                    f'running {len(cases)} tests\n' + ''.join(f'test {case} ... {result}\n' for case, result in cases.items())
                    + f'\ntest result: ok. {len(cases)} passed; 0 failed; 0 ignored; 0 measured; 0 filtered out\n'
                )
                xml = log.with_suffix('.xml')
                subprocess.run([sys.executable, str(TOOLS / 'junit_xml.py'), str(xml), label, '0', '9.000', str(log)], check=True)
                results[label] = {'outputs': {'junit_xml': str(xml)}}
            path = self.root / f'{name}.json'
            path.write_text(json.dumps({'results': results}))
            return path

        first = report('first', {
            'root//pkg:t__shard_1': {'a::slow': 'ok <12.345s>', 'a::untimed': 'ok'},
            'root//pkg:t__shard_2': {'a::quick': 'ok <0.0001s>'},
            'root//pkg:t__fv_0873c7ac__shard_1': {'a::slow': 'ok <30.000s>'},
            'root//pkg:whole': {'b::case': 'ok <1.000s>'},
        })
        second = report('second', {'root//pkg:t__shard_1': {'a::slow': 'ok <10.000s>'}})
        xml = ET.parse(self.root / 'first-0.xml').getroot()
        self.assertEqual({case.get('name'): case.get('time') for case in xml.iter('testcase')}, {'a::slow': '12.345', 'a::untimed': None})
        times = shard_weights.measured([first, second])
        self.assertEqual(times, {'//pkg:t': {'a::slow': [12.345, 30.0, 10.0], 'a::quick': [0.0001]}})
        table = shard_weights.refreshed({'//pkg:kept': {'x': 7}, '//pkg:t': {'gone': 3}}, times)
        self.assertEqual(table, {'//pkg:kept': {'x': 7}, '//pkg:t': {'a::slow': 10000, 'a::quick': 1}})
        self.assertEqual(shard_weights.plan(table, '//pkg:t', 2), ['shard 1/2: 1 cases, 10000 ms', 'shard 2/2: 1 cases, 1 ms'])
        with self.assertRaisesRegex(SystemExit, '--report-time: //pkg:t$'):
            shard_weights.refreshed({}, shard_weights.measured([report('untimed', {'root//pkg:t__shard_1': {'a::slow': 'ok'}})]))


if __name__ == '__main__':
    unittest.main()
