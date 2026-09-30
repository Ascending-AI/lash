#!/usr/bin/env python3
import copy
import contextlib
import io
import json
import tempfile
import unittest
from pathlib import Path

import loadtest_measurements as m
from loadtest_resources import descendants
from loadtest_storage import physical


def operation(key='one', outcome='answered', **fields):
    return dict(schema_version=1, record='operation', run='r', id=key, invocation_id=key,
                subject_id=key, request={'synthetic': key}, response={'status': outcome}, error=None,
                scenario='turn', phase='smoke', scheduled_ns=0, sent_ns=10, accepted_ns=20,
                observed_ns=100, outcome=outcome, client_attempts=1, journal={'entries': 3, 'bytes': 40},
                **fields)


def evidence():
    run = dict(schema_version=1, record='run', run='r', workers=['worker'], sessions=1, turns_per_session=1)
    witness = dict(schema_version=1, record='witness', run='r', sent=1, terminal=1,
                   provider_calls=1, provider_retryable_failures=0, effect_attempts=1, effect_commits=1,
                   verdict={'absorbed_effect_attempts': 0, 'classes': {name: {'witnessed': 1, 'violations': []} for name in m.WITNESS_CLASSES}})
    resources = {'processes': [{'pid': 1, 'parent_pid': 0, 'epoch': '1', 'rss_bytes': 30, 'cpu_ticks': 6}],
                 'rss_sum_bytes': 30, 'cpu_ticks_sum': 6, 'cgroup_memory_bytes': 100, 'cgroup_peak_bytes': 100,
                 'cgroup_cpu': {'usage_usec': 10, 'throttled_usec': 0, 'nr_throttled': 0},
                 'cgroup_events': {'oom_kill': 0}, 'clock_ticks_per_second': 100}
    sample = dict(schema_version=1, record='sample', run='r', monotonic_ns=0, collection_finished_ns=5,
                  workers=[{'node': 'worker', 'resources': resources}],
                  postgres=dict(epoch='1', stats_reset='1', wal_reset='1', query_reset='1',
                                transactions=1, blocks_read=1, blocks_hit=1, read_ms=0, write_ms=0,
                                wal_bytes=10, query_calls=1, query_ms=1, connections=1, waiters=0, lock_waiters=0),
                  invocations=[], journals=[],
                  restate=[dict(node=f'node-{index}',
                                prometheus='restate_invoker_invocation_tasks_total{partition_id="1",status="started"} 7',
                                physical={'allocated_bytes': 100, 'directories': {'log': 100}, 'snapshot': {'bytes': 5}}) for index in range(3)],
                  node_epochs=[dict(name=f'n{index}', gen_node_id=f'{index}:1', address=f'node-{index}') for index in range(3)],
                  leader_epochs=[dict(partition_id=1, leader_epoch='1', leader_gen_node_id='0:1')])
    final = copy.deepcopy(sample)
    final.update(monotonic_ns=110, collection_finished_ns=120,
                 invocations=[dict(id='one', target_service_key='load-r-one', invoked_by_id=None, status='completed', retry_count=90)],
                 journals=[{'id': 'one', 'journal': {'entries': 3, 'bytes': 40}}])
    final['restate'][0]['prometheus'] = final['restate'][0]['prometheus'].replace(' 7', ' 8')
    return run, [operation()], [sample, final], witness


def witness_evidence(operations=None):
    operations = operations if operations is not None else [operation()]
    events = []
    for row in operations:
        for phase in ['sent', 'terminal']:
            detail = dict(request=row['request'], scheduled_ns=str(row['scheduled_ns']), sent_ns=str(row['sent_ns'])) if phase == 'sent' else dict(response=row['response'], error=row['error'], terminal_ns=str(row['observed_ns']))
            events.append(dict(subject=row['subject_id'], operation=row['scenario'], observer='driver', phase=phase, detail_json=json.dumps(detail)))
    return [dict(schema_version=1, record='witness_evidence', run='r', ledger=name,
                 rows=events if name == 'witness_load_events' else [] if name in {'witness_effect_replies', 'witness_load_faults'} else [{'id': 'one'}])
            for name in sorted(m.LEDGERS)]


class MeasurementsTests(unittest.TestCase):
    def test_all_behavior_classes_are_required(self):
        required = ('provider-streams', 'history-prefill', 'admin-compaction', 'context-pressure',
                    'auxiliary-requests', 'external-occurrences', 'trigger-edits', 'promotion-reads')
        run, operations, samples, witness = evidence()
        m.summarize(run, operations, samples, witness)
        for name in required:
            with self.subTest(name=name):
                self.assertIn(name, witness['verdict']['classes'])
                missing = copy.deepcopy(witness)
                del missing['verdict']['classes'][name]
                with self.assertRaisesRegex(ValueError, 'missing or unknown durable evidence classes'):
                    m.summarize(run, operations, samples, missing)

    def test_cancellation_and_timeout_are_retained(self):
        result = m.populations([operation(), operation('two', 'cancelled'), operation('three', 'timeout')])
        self.assertEqual(result['offered'], 3)
        self.assertEqual(result['durable_terminal'], 2)
        self.assertEqual(result['outcomes'], {'answered': 1, 'cancelled': 1, 'timeout': 1})

    def test_admission_is_not_completion(self):
        self.assertEqual(m.populations([operation(outcome='parked')])['durable_terminal'], 0)

    def test_duplicate_operation_fails(self):
        with self.assertRaises(ValueError):
            m.populations([operation(), operation()])

    def test_monotonic_clock_order_fails(self):
        row = operation()
        row['sent_ns'] = 110
        with self.assertRaises(ValueError):
            m.populations([row])

    def test_missing_terminal_fails(self):
        row = operation()
        del row['observed_ns']
        with self.assertRaises(ValueError):
            m.populations([row])

    def test_missing_accepted_is_valid_for_rejection(self):
        row = operation(outcome='client_error')
        row['accepted_ns'] = None
        self.assertEqual(m.populations([row])['accepted'], 0)

    def test_retry_snapshots_are_not_summed(self):
        total = m.CounterDeltas()
        self.assertEqual(total.add('retry', 'node-1', 7), 0)
        self.assertEqual(total.add('retry', 'node-1', 9), 2)
        self.assertEqual(total.total, 2)

    def test_node_reset_without_epoch_fails(self):
        total = m.CounterDeltas()
        total.add('retry', 'node-1', 7)
        with self.assertRaises(ValueError):
            total.add('retry', 'node-1', 0)

    def test_new_node_epoch_is_a_baseline_and_marks_gap(self):
        total = m.CounterDeltas()
        total.add('retry', 'node-1', 7)
        self.assertEqual(total.add('retry', 'node-2', 3), 0)
        self.assertEqual(total.gaps, 1)

    def test_negative_and_nonfinite_counters_fail(self):
        for value in [-1, float('nan'), float('inf')]:
            with self.assertRaises(ValueError):
                m.CounterDeltas().add('retry', 'node-1', value)

    def test_child_resource_sum_excludes_unrelated_process(self):
        rows = [{'pid': 1, 'parent_pid': 0}, {'pid': 2, 'parent_pid': 1},
                {'pid': 3, 'parent_pid': 2}, {'pid': 4, 'parent_pid': 0}]
        self.assertEqual([row['pid'] for row in descendants(rows, 1)], [1, 2, 3])

    def test_resource_totals_and_unique_pids(self):
        resources = {'processes': [{'pid': 1, 'rss_bytes': 10, 'cpu_ticks': 2},
                                   {'pid': 2, 'rss_bytes': 20, 'cpu_ticks': 4}],
                     'rss_sum_bytes': 30, 'cpu_ticks_sum': 6, 'cgroup_memory_bytes': 100}
        m.validate_resources(resources)
        resources['rss_sum_bytes'] = 31
        with self.assertRaises(ValueError):
            m.validate_resources(resources)

    def test_cgroup_is_not_added_to_process_rss(self):
        self.assertEqual(m.deployment_memory([{'cgroup_memory_bytes': 100, 'rss_sum_bytes': 60},
                                             {'cgroup_memory_bytes': 200, 'rss_sum_bytes': 120}]), 300)

    def test_growth_uses_per_invocation_maxima(self):
        self.assertEqual(m.journal_summary([
            {'id': 'a', 'journal': {'entries': 5, 'bytes': 70}},
            {'id': 'a', 'journal': {'entries': 2, 'bytes': 40}},
            {'id': 'b', 'journal': {'entries': 3, 'bytes': 20}},
        ], 2)['bytes_per_terminal'], 45)

    def test_physical_bytes_ignore_symlinks(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            (root / 'data').write_bytes(b'x' * 5000)
            (root / 'link').symlink_to(root / 'data')
            self.assertEqual(physical(root)['allocated_bytes'], (root / 'data').stat().st_blocks * 512)

    def test_prometheus_labels_and_nan(self):
        parsed = m.prometheus('restate_invoker_invocation_tasks_total{partition_id="2",status="failed",transient="true"} 7\n# HELP ignored text\n')
        self.assertEqual(parsed[0][1]['partition_id'], '2')
        self.assertEqual(parsed[0][2], 7)
        with self.assertRaises(ValueError):
            m.prometheus('x NaN')

    def test_recovery_requires_terminals_and_durability(self):
        fault = {'schema_version': 1, 'id': 'f', 'actual_ns': 50, 'service_progress_ns': 60,
                 'backlog_recovered_ns': 100, 'witness_verdict': 'passed', 'accepted_ids': ['one']}
        self.assertEqual(m.recoveries([fault], [operation()])[0]['backlog_recovery_ns'], 50)
        fault['accepted_ids'] = ['lost']
        with self.assertRaises(ValueError):
            m.recoveries([fault], [operation()])

    def test_recovery_does_not_count_client_timeout_as_terminal(self):
        fault = {'schema_version': 1, 'id': 'f', 'actual_ns': 50, 'service_progress_ns': 60,
                 'backlog_recovered_ns': 100, 'witness_verdict': 'passed', 'accepted_ids': ['one']}
        with self.assertRaises(ValueError):
            m.recoveries([fault], [operation(outcome='timeout')])

    def test_tail_percentile_uses_all_population_samples(self):
        result = m.distribution(list(range(1, 101)))
        self.assertEqual([result[key] for key in ('p50', 'p95', 'p99')], [50, 95, 99])

    def test_version_rejected(self):
        row = operation()
        row['schema_version'] = 2
        with self.assertRaises(ValueError):
            m.populations([row])

    def test_complete_pipeline_reconciles_and_keeps_pool_pending(self):
        result = m.summarize(*evidence())
        self.assertEqual(result['population']['durable_terminal'], 1)
        self.assertEqual(result['pool']['status'], 'PENDING')
        self.assertIsNone(result['baseline'])
        self.assertEqual(result['shared_snapshot_peak_bytes'], 5)

    def test_population_missing_witness_fails(self):
        run, operations, samples, witness = evidence()
        witness['terminal'] = 0
        with self.assertRaises(ValueError):
            m.summarize(run, operations, samples, witness)

    def test_missing_worker_or_node_fails(self):
        for key in ['workers', 'restate']:
            run, operations, samples, witness = evidence()
            samples[-1][key].pop()
            with self.assertRaises(ValueError):
                m.summarize(run, operations, samples, witness)

    def test_completed_descendant_must_have_journal_evidence(self):
        run, operations, samples, witness = evidence()
        samples[-1]['invocations'].append(dict(id='child', target_service_key='child', invoked_by_id='one', status='completed'))
        result = m.summarize(run, operations, samples, witness)
        self.assertEqual(result['qualification']['status'], 'INCOMPLETE')
        self.assertEqual(result['qualification']['missing_journal_ids'], ['child'])

    def test_http_started_body_is_included_by_isolated_census(self):
        run, operations, samples, witness = evidence()
        samples[-1]['invocations'].append(dict(id='body', target_service_key='opaque-process', invoked_by_id=None, status='completed'))
        samples[-1]['journals'].append(dict(id='body', journal={'entries': 1, 'bytes': 20}))
        self.assertEqual(m.summarize(run, operations, samples, witness)['journals']['invocations'], 2)

    def test_first_retry_series_is_not_dropped(self):
        run, operations, samples, witness = evidence()
        samples[-1]['restate'][0]['prometheus'] += '\nrestate_invoker_invocation_tasks_total{partition_id="1",status="failed",transient="true"} 1'
        result = m.summarize(run, operations, samples, witness)
        self.assertEqual(result['counters']['restate_task_failed_retryable']['observed_delta'], 1)

    def test_leader_handoff_keeps_cumulative_node_delta(self):
        run, operations, samples, witness = evidence()
        samples[-1]['leader_epochs'][0]['leader_epoch'] = '2'
        samples[-1]['restate'][0]['prometheus'] = samples[-1]['restate'][0]['prometheus'].replace(' 8', ' 9')
        result = m.summarize(run, operations, samples, witness)
        self.assertEqual(result['counters']['restate_task_started']['observed_delta'], 2)
        self.assertEqual(result['retry_epochs'][-1]['leader_epoch'], '2')

    def test_unobserved_node_epoch_gap_fails_completeness(self):
        run, operations, samples, witness = evidence()
        samples[-1]['node_epochs'][0]['gen_node_id'] = '0:2'
        with self.assertRaises(ValueError):
            m.summarize(run, operations, samples, witness)

    def test_required_missing_counter_fails(self):
        run, operations, samples, witness = evidence()
        samples[-1]['restate'][0]['prometheus'] = ''
        with self.assertRaises(ValueError):
            m.summarize(run, operations, samples, witness)

    def test_metrics_must_cover_admission_and_drain(self):
        run, operations, samples, witness = evidence()
        samples[-1]['monotonic_ns'] = 99
        with self.assertRaises(ValueError):
            m.summarize(run, operations, samples, witness)

    def test_metric_rows_have_explicit_units_and_clock_uncertainty(self):
        rows = list(m.metric_rows(evidence()[2]))
        self.assertTrue(rows)
        self.assertTrue(all(row['schema_version'] == 1 and row['unit'] and 'clock_uncertainty_ns' in row for row in rows))
        snapshots = [row for row in rows if row['metric'] == 'snapshot_objects_bytes']
        self.assertEqual(len(snapshots), 2)
        self.assertEqual([row['value'] for row in snapshots], [5, 5])

    def test_omitted_evidence_class_fails(self):
        run, operations, samples, witness = evidence()
        del witness['verdict']['classes']['tools']
        with self.assertRaises(ValueError):
            m.summarize(run, operations, samples, witness)

    def test_recovery_cannot_omit_an_accepted_input(self):
        fault = {'schema_version': 1, 'id': 'f', 'actual_ns': 50, 'service_progress_ns': 60,
                 'backlog_recovered_ns': 100, 'witness_verdict': 'passed', 'accepted_ids': []}
        with self.assertRaises(ValueError):
            m.recoveries([fault], [operation()])

    def test_collection_failure_is_retained_and_fails_completeness(self):
        with self.assertRaises(ValueError):
            m.summarize(*evidence(), sample_errors=[{'record': 'sample_error'}])

    def test_pruned_completed_journal_fails(self):
        run, operations, samples, witness = evidence()
        operations[0]['journal'] = {'entries': 0, 'bytes': 0}
        samples[-1]['journals'][0]['journal'] = {'entries': 0, 'bytes': 0}
        result = m.summarize(run, operations, samples, witness)
        self.assertEqual(result['qualification']['status'], 'INCOMPLETE')
        self.assertEqual(result['invariants']['completed_journals'], 'INCOMPLETE')

    def test_idle_node_absent_counter_series_is_zero(self):
        run, operations, samples, witness = evidence()
        for sample in samples:
            sample['restate'][2]['prometheus'] = 'restate_num_active_partitions 0'
        self.assertEqual(m.summarize(run, operations, samples, witness)['counters']['restate_task_started']['observed_delta'], 1)

    def test_started_counter_must_cover_accepted_operations(self):
        run, operations, samples, witness = evidence()
        samples[-1]['restate'][0]['prometheus'] = samples[-1]['restate'][0]['prometheus'].replace(' 8', ' 7')
        with self.assertRaises(ValueError):
            m.summarize(run, operations, samples, witness)

    def test_archive_emits_versioned_files_without_baseline(self):
        run, operations, samples, witness = evidence()
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / 'load.log'
            rows = [run, *operations, *samples, *witness_evidence(), witness]
            log.write_text(''.join('load measurement ' + json.dumps(row) + '\n' for row in rows))
            with contextlib.redirect_stdout(io.StringIO()):
                m.archive(log, Path(tmp) / 'results')
            output = Path(tmp) / 'results' / 'fig-3790' / 'r'
            self.assertEqual({path.name for path in output.iterdir()},
                             {'operations.jsonl', 'samples.jsonl', 'sample_errors.jsonl', 'metrics.jsonl',
                              'faults.jsonl', 'summary.json', 'histograms.json', 'collection.json',
                              'witness.json', 'witness_evidence.jsonl', 'query_retries.jsonl'})
            for path in output.iterdir():
                values = [json.loads(line) for line in path.read_text().splitlines()] if path.suffix == '.jsonl' else [json.loads(path.read_text())]
                self.assertTrue(all(row['schema_version'] == 1 for row in values))
            self.assertIsNone(json.loads((output / 'summary.json').read_text())['baseline'])

    def test_witness_identities_reconcile_beyond_matching_counts(self):
        run, operations, _, witness = evidence()
        rows = witness_evidence()
        m.reconcile_witness(run, operations, witness, rows)
        events = next(row['rows'] for row in rows if row['ledger'] == 'witness_load_events')
        events[0]['subject'] = 'another'
        with self.assertRaisesRegex(ValueError, 'identities differ'):
            m.reconcile_witness(run, operations, witness, rows)

    def test_witness_receipt_counters_require_exact_rows(self):
        run, operations, _, witness = evidence()
        rows = witness_evidence()
        next(row['rows'] for row in rows if row['ledger'] == 'witness_provider_receipts').clear()
        with self.assertRaisesRegex(ValueError, 'counter mismatch'):
            m.reconcile_witness(run, operations, witness, rows)

    def test_witness_timestamp_mismatch_fails(self):
        run, operations, _, witness = evidence()
        rows = witness_evidence()
        event = next(row['rows'] for row in rows if row['ledger'] == 'witness_load_events')[1]
        detail = json.loads(event['detail_json'])
        detail['terminal_ns'] = '101'
        event['detail_json'] = json.dumps(detail)
        with self.assertRaisesRegex(ValueError, 'timestamp differs'):
            m.reconcile_witness(run, operations, witness, rows)

    def test_witness_response_mismatch_fails(self):
        run, operations, _, witness = evidence()
        rows = witness_evidence()
        event = next(row['rows'] for row in rows if row['ledger'] == 'witness_load_events')[1]
        detail = json.loads(event['detail_json'])
        detail['response'] = {'status': 'cancelled'}
        event['detail_json'] = json.dumps(detail)
        with self.assertRaisesRegex(ValueError, 'terminal differs'):
            m.reconcile_witness(run, operations, witness, rows)

    def test_sampled_park_occupancy_uses_transition_intervals(self):
        run, operations, samples, witness = evidence()
        middle = copy.deepcopy(samples[-1])
        middle.update(monotonic_ns=50, collection_finished_ns=55)
        middle['invocations'][0]['status'] = 'suspended'
        result = m.summarize(run, operations, [samples[0], middle, samples[1]], witness)
        self.assertEqual(result['parks']['sampled_transitions'], 1)
        self.assertEqual(result['parks']['sampled_occupancy_ns'], 60)

    def test_failed_archive_keeps_exact_evidence_and_settings(self):
        run, operations, samples, witness = evidence()
        run['journal_settings'] = {'journal_retention': '5m'}
        witness['verdict']['classes']['queued-inputs']['violations'] = ['wrong final value']
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / 'measurements.log'
            log.write_text(''.join('load measurement ' + json.dumps(row) + '\n' for row in [run, *operations, *samples, *witness_evidence(), witness]))
            with self.assertRaisesRegex(ValueError, 'durability witness failed'):
                m.archive(log, Path(tmp) / 'results')
            output = Path(tmp) / 'results' / 'fig-3790' / 'r'
            self.assertEqual(json.loads((output / 'summary.json').read_text())['verdict'], 'failed')
            self.assertEqual(json.loads((output / 'collection.json').read_text())['journal_settings']['journal_retention'], '5m')
            self.assertEqual(len((output / 'witness_evidence.jsonl').read_text().splitlines()), 6)
            self.assertEqual(json.loads((output / 'witness.json').read_text()), witness)

    def test_archive_rejects_unknown_record_contract(self):
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / 'measurements.log'
            log.write_text('load measurement ' + json.dumps({'schema_version': 1, 'record': 'future_counter'}))
            with self.assertRaisesRegex(ValueError, 'unsupported measurement record'):
                m.read_records(log)

    def test_archive_rejects_foreign_retry_run(self):
        run, operations, samples, witness = evidence()
        retry = dict(schema_version=1, record='query_retry', run='another-run')
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / 'measurements.log'
            log.write_text(''.join('load measurement ' + json.dumps(row) + '\n' for row in [run, *operations, *samples, *witness_evidence(), witness, retry]))
            with self.assertRaisesRegex(ValueError, 'mixed archive run identities'):
                m.archive(log, Path(tmp) / 'results')

    def test_incomplete_archive_keeps_metrics_and_exits_unsuccessfully(self):
        run, operations, samples, witness = evidence()
        samples[-1]['invocations'].append(dict(id='parked-child', target_service_key='child', invoked_by_id='one', status='suspended'))
        samples[-1]['journals'].append(dict(id='parked-child', journal={'entries': 2, 'bytes': 20}))
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / 'measurements.log'
            log.write_text(''.join('load measurement ' + json.dumps(row) + '\n' for row in [run, *operations, *samples, *witness_evidence(), witness]))
            with contextlib.redirect_stdout(io.StringIO()), self.assertRaisesRegex(ValueError, 'qualification is INCOMPLETE'):
                m.archive(log, Path(tmp) / 'results')
            output = Path(tmp) / 'results' / 'fig-3790' / 'r'
            summary = json.loads((output / 'summary.json').read_text())
            self.assertEqual(summary['qualification']['status'], 'INCOMPLETE')
            self.assertEqual(summary['invariants']['resource_totals'], 'passed')
            self.assertTrue((output / 'metrics.jsonl').is_file())
            self.assertTrue((output / 'histograms.json').is_file())

    def test_disappeared_invocation_is_not_excused_as_cancelled(self):
        run, operations, samples, witness = evidence()
        middle = copy.deepcopy(samples[-1])
        middle.update(monotonic_ns=50, collection_finished_ns=55)
        middle['invocations'].append(dict(id='inboxed-child', target_service_key='child', invoked_by_id='one', status='inboxed'))
        middle['journals'].append(dict(id='inboxed-child', journal={'entries': 0, 'bytes': 0}))
        result = m.summarize(run, operations, [samples[0], middle, samples[1]], witness)
        self.assertEqual(result['qualification']['status'], 'INCOMPLETE')
        self.assertEqual(result['qualification']['disappeared_invocations'][0]['status'], 'inboxed')

    def cancelled_inbox_evidence(self):
        run, operations, samples, witness = evidence()
        middle = copy.deepcopy(samples[-1])
        middle.update(monotonic_ns=50, collection_finished_ns=55)
        middle['invocations'].append(dict(id='inboxed-child', target_service_name='EffectGroupIndex', target_service_key='child', invoked_by_id='one', status='inboxed'))
        middle['journals'].append(dict(id='inboxed-child', journal={'entries': 0, 'bytes': 0}))
        for sample in [samples[0], middle, samples[1]]:
            sample['cancellation_commands'] = []
        entry = dict(target_invocation_id='inboxed-child', signal_id={'Index': 1}, result='Void', name='')
        samples[1]['cancellation_commands'] = [dict(id='one', index=9, entry_type='Command: SendSignal', version=2,
                                                   entry_json=json.dumps({'Command': {'SendSignal': entry}}))]
        return run, operations, [samples[0], middle, samples[1]], witness

    def test_recorded_cancel_explains_disappeared_inboxed_child(self):
        result = m.summarize(*self.cancelled_inbox_evidence())
        self.assertEqual(result['qualification']['status'], 'PASSED')
        self.assertEqual(result['cancellations']['explained_inbox_ids'], ['inboxed-child'])

    def test_other_signal_does_not_explain_disappeared_child(self):
        run, operations, samples, witness = self.cancelled_inbox_evidence()
        entry = json.loads(samples[-1]['cancellation_commands'][0]['entry_json'])
        entry['Command']['SendSignal']['signal_id'] = {'Name': 'wake'}
        samples[-1]['cancellation_commands'][0]['entry_json'] = json.dumps(entry)
        result = m.summarize(run, operations, samples, witness)
        self.assertEqual(result['qualification']['status'], 'FAILED')
        self.assertEqual(result['cancellations']['unexplained_disappearance_ids'], ['inboxed-child'])

    def test_cancel_from_unowned_sender_does_not_explain_disappearance(self):
        run, operations, samples, witness = self.cancelled_inbox_evidence()
        samples[-1]['cancellation_commands'][0]['id'] = 'unrelated'
        with self.assertRaisesRegex(ValueError, 'unowned cancellation sender'):
            m.summarize(run, operations, samples, witness)

    def test_recorded_cancel_does_not_excuse_vanished_running_child(self):
        run, operations, samples, witness = self.cancelled_inbox_evidence()
        samples[1]['invocations'][-1]['status'] = 'running'
        result = m.summarize(run, operations, samples, witness)
        self.assertEqual(result['qualification']['status'], 'FAILED')

    def test_repeated_cancel_command_is_counted_once(self):
        run, operations, samples, witness = self.cancelled_inbox_evidence()
        samples[1]['cancellation_commands'] = copy.deepcopy(samples[-1]['cancellation_commands'])
        result = m.summarize(run, operations, samples, witness)
        self.assertEqual(result['cancellations']['commands'], 1)

    def test_changed_cancel_command_is_rejected(self):
        run, operations, samples, witness = self.cancelled_inbox_evidence()
        samples[1]['cancellation_commands'] = copy.deepcopy(samples[-1]['cancellation_commands'])
        samples[-1]['cancellation_commands'][0]['entry_json'] = '{}'
        with self.assertRaisesRegex(ValueError, 'journal cancellation command changed'):
            m.summarize(run, operations, samples, witness)

    def test_legacy_cancel_projection_remains_incomplete(self):
        run, operations, samples, witness = self.cancelled_inbox_evidence()
        samples[-1]['cancellation_commands'][0].update(version=1, entry_json=None)
        result = m.summarize(run, operations, samples, witness)
        self.assertEqual(result['qualification']['status'], 'INCOMPLETE')
        self.assertFalse(result['cancellations']['available'])

    def test_malformed_cancel_projection_is_rejected(self):
        run, operations, samples, witness = self.cancelled_inbox_evidence()
        samples[-1]['cancellation_commands'][0]['entry_json'] = '{}'
        with self.assertRaisesRegex(ValueError, 'invalid cancellation journal projection'):
            m.summarize(run, operations, samples, witness)

    def campaign_evidence(self):
        run, operations, samples, witness = evidence()
        run.update(fault_campaign=True, mode='fault_campaign')
        operations.append(operation('two'))
        for ordinal, row in enumerate(operations):
            row.update(actor_id=0, request={'ordinal': ordinal})
        witness.update(sent=2, terminal=2)
        witness['verdict']['classes'].update({name: {'witnessed': 1, 'violations': []}
                                             for name in m.FAULT_CLASSES})
        samples[-1]['invocations'].append(dict(id='two', target_service_key='load-r-two',
                                              invoked_by_id=None, status='completed'))
        samples[-1]['restate'][0]['prometheus'] = samples[-1]['restate'][0]['prometheus'].replace(' 8', ' 9')
        return run, operations, samples, witness

    def test_campaign_extra_turns_require_recovery_clock_normalization(self):
        result = m.summarize(*self.campaign_evidence())
        self.assertEqual(result['population']['durable_terminal'], 2)
        self.assertEqual(result['mode'], 'fault_campaign')
        self.assertEqual(result['qualification']['status'], 'INCOMPLETE')
        self.assertEqual(result['qualification']['reasons'],
                         ['L4 witness-clock faults require driver-clock recovery normalization'])

    def test_campaign_requires_all_fault_witness_classes(self):
        run, operations, samples, witness = self.campaign_evidence()
        del witness['verdict']['classes']['rolling-deploy']
        with self.assertRaisesRegex(ValueError, 'missing or unknown durable evidence classes'):
            m.summarize(run, operations, samples, witness)

    def test_campaign_rejects_missing_primary_ordinal(self):
        run, operations, samples, witness = self.campaign_evidence()
        operations[-1]['request']['ordinal'] = 2
        with self.assertRaisesRegex(ValueError, 'campaign primary ordinals are incomplete'):
            m.summarize(run, operations, samples, witness)

    def test_independent_fault_ledger_must_match_witness_count(self):
        run, operations, _, witness = evidence()
        witness['fault_rows'] = 1
        with self.assertRaisesRegex(ValueError, 'fault witness counter mismatch'):
            m.reconcile_witness(run, operations, witness, witness_evidence(operations))


if __name__ == '__main__':
    unittest.main()
