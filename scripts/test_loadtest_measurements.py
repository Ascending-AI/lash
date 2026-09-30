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
                  workers=[{'node': 'worker', 'observed_ns': 5, 'resources': resources}],
                  postgres=dict(epoch='1', stats_reset='1', wal_reset='1', query_reset='1',
                                transactions=1, blocks_read=1, blocks_hit=1, read_ms=0, write_ms=0,
                                wal_bytes=10, query_calls=1, query_ms=1, connections=1, waiters=0, lock_waiters=0),
                  invocations=[], journals=[],
                  restate=[dict(node=f'node-{index}', metrics_observed_ns=5,
                                prometheus='restate_invoker_invocation_tasks_total{partition_id="1",status="started"} 7',
                                physical={'allocated_bytes': 100, 'directories': {'log': 100}, 'snapshot': {'bytes': 5}}) for index in range(3)],
                  node_epochs=[dict(name=f'n{index}', gen_node_id=f'{index}:1', address=f'node-{index}') for index in range(3)],
                  leader_epochs=[dict(partition_id=1, leader_epoch='1', leader_gen_node_id='0:1')])
    final = copy.deepcopy(sample)
    final.update(monotonic_ns=110, collection_finished_ns=120,
                 invocations=[dict(id='one', target_service_key='load-r-one', invoked_by_id=None, status='completed', retry_count=90)],
                 journals=[{'id': 'one', 'journal': {'entries': 3, 'bytes': 40}}])
    final['workers'][0]['observed_ns'] = 120
    for node in final['restate']:
        node['metrics_observed_ns'] = 120
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

    def test_fault_clock_offsets_and_bounds_are_exact(self):
        anchor = dict(schema_version=1, record='clock_anchor', run='r', id='7',
                      clock='witness_postgres_us', witness_us=9000000000000,
                      monotonic_ns=10000, round_trip_ns=2000)
        row = dict(fault_event_id=7, run_id='r', fault_id='worker-kill', kind='worker-kill',
                   phase='injected', recorded_at_us=9000000000003, detail_json='{}')
        normalized, errors = m.normalize_fault_rows([row], [anchor], 'r')
        self.assertEqual(errors, [])
        self.assertEqual(normalized[0]['monotonic_ns'], 13000)
        self.assertEqual(normalized[0]['clock_bounds_ns'], [11000, 15000])
        self.assertEqual(normalized[0]['clock_uncertainty_ns'], 2000)
        self.assertEqual(normalized[0]['anchor_id'], '7')

    def test_fault_without_anchor_names_row_and_stays_incomplete(self):
        run, operations, samples, witness = self.campaign_evidence()
        row = dict(fault_event_id=7, run_id='r', fault_id='worker-kill', kind='worker-kill',
                   phase='injected', recorded_at_us=9000000000003, detail_json='{}')
        result = m.summarize(run, operations, samples, witness, faults=[row])
        self.assertEqual(result['qualification']['status'], 'INCOMPLETE')
        self.assertIn('worker-kill/injected/7: missing clock anchor', result['qualification']['reasons'])
        self.assertEqual(result['recovery_inputs']['status'], 'INCOMPLETE')

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
                 'backlog_recovered_ns': 100, 'clock': 'driver_monotonic_ns', 'actual_bounds_ns': [48, 52],
                 'service_progress_bounds_ns': [58, 62], 'backlog_recovered_bounds_ns': [98, 102],
                 'anchor_ids': ['injected', 'recovered'], 'witness_verdict': 'passed', 'accepted_ids': ['one']}
        self.assertEqual(m.recoveries([fault], [operation()])[0]['backlog_recovery_ns'], 50)
        fault['accepted_ids'] = ['lost']
        with self.assertRaises(ValueError):
            m.recoveries([fault], [operation()])

    def test_recovery_does_not_count_client_timeout_as_terminal(self):
        fault = {'schema_version': 1, 'id': 'f', 'actual_ns': 50, 'service_progress_ns': 60,
                 'backlog_recovered_ns': 100, 'clock': 'driver_monotonic_ns', 'actual_bounds_ns': [48, 52],
                 'service_progress_bounds_ns': [58, 62], 'backlog_recovered_bounds_ns': [98, 102],
                 'anchor_ids': ['injected', 'recovered'], 'witness_verdict': 'passed', 'accepted_ids': ['one']}
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
                 'backlog_recovered_ns': 100, 'clock': 'driver_monotonic_ns', 'actual_bounds_ns': [48, 52],
                 'service_progress_bounds_ns': [58, 62], 'backlog_recovered_bounds_ns': [98, 102],
                 'anchor_ids': ['injected', 'recovered'], 'witness_verdict': 'passed', 'accepted_ids': []}
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
                              'witness.json', 'witness_evidence.jsonl', 'query_retries.jsonl',
                              'clock_anchors.jsonl', 'recovery_inputs.jsonl', 'recovery.json', 'normalized_faults.jsonl',
                              'collection_gaps.jsonl'})
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

    def normalized_campaign(self):
        rows = []
        anchors = [dict(schema_version=1, record='clock_anchor', run='r', id='campaign-start',
                        clock='witness_postgres_us', witness_us=950, monotonic_ns=0, round_trip_ns=2000)]
        for event, phase, at in [(1, 'injected', 50), (2, 'recovered', 110)]:
            detail = dict(service_progress_at_us=1010, backlog_recovered_at_us=1060) if phase == 'recovered' else {}
            rows.append(dict(fault_event_id=event, run_id='r', fault_id='worker-kill', kind='worker-kill',
                             phase=phase, recorded_at_us=950 + at, detail_json=json.dumps(detail)))
            anchors.append(dict(schema_version=1, record='clock_anchor', run='r', id=str(event),
                                clock='witness_postgres_us', witness_us=950 + at, monotonic_ns=at * 1000,
                                round_trip_ns=2000))
        return rows, anchors

    def gap_campaign(self):
        run, operations, samples, witness = self.campaign_evidence()
        for row in operations:
            for key in ['scheduled_ns', 'sent_ns', 'accepted_ns', 'observed_ns']:
                row[key] *= 1000
        for sample in samples:
            for key in ['monotonic_ns', 'collection_finished_ns']:
                sample[key] *= 1000
            for worker in sample['workers']:
                worker['observed_ns'] *= 1000
            for node in sample['restate']:
                node['metrics_observed_ns'] *= 1000
        faults, anchors = self.normalized_campaign()
        faults[0]['detail_json'] = json.dumps({'collection_targets': [
            {'component': 'worker', 'endpoint': 'worker'}]})
        gap = dict(schema_version=1, record='sample_error', run='r', sample_kind='periodic',
                   monotonic_ns=80000, error='request timed out',
                   target={'component': 'worker', 'endpoint': 'worker'})
        return run, operations, samples, witness, faults, anchors, gap

    def gap_summary(self, fixture):
        run, operations, samples, witness, faults, anchors, gap = fixture
        return m.summarize(run, operations, samples, witness, faults=faults,
                           anchors=anchors, sample_errors=[gap])

    def test_in_window_faulted_target_gap_is_attributed(self):
        fixture = self.gap_campaign()
        result = self.gap_summary(fixture)
        self.assertEqual(result['qualification']['status'], 'PASSED')
        gap = result['collection_gaps'][0]
        self.assertEqual(gap['error'], 'request timed out')
        self.assertEqual(gap['attribution']['status'], 'FAULT_ATTRIBUTED')
        self.assertEqual(gap['attribution']['fault_id'], 'worker-kill')
        self.assertEqual(gap['attribution']['anchor_ids'], ['1', '2'])
        self.assertEqual(gap['attribution']['window_ns'], [52000, 108000])
        self.assertEqual(gap['target'], fixture[-1]['target'])

    def test_out_of_window_gap_fails(self):
        for instant in [0, 51999, 108001, 200000]:
            with self.subTest(instant=instant):
                fixture = self.gap_campaign()
                fixture[-1]['monotonic_ns'] = instant
                result = self.gap_summary(fixture)
                self.assertEqual(result['qualification']['status'], 'FAILED')
                self.assertEqual(result['collection_gaps'][0]['attribution']['status'], 'UNATTRIBUTED')
                self.assertIn('required collection intervals are missing', result['qualification']['reasons'])

    def test_wrong_component_gap_fails(self):
        for target in [{'component': 'restate', 'endpoint': 'worker'},
                       {'component': 'postgres', 'endpoint': 'database'},
                       {'component': 'worker', 'endpoint': 'other-worker'}]:
            with self.subTest(target=target):
                fixture = self.gap_campaign()
                fixture[-1]['target'] = target
                result = self.gap_summary(fixture)
                self.assertEqual(result['qualification']['status'], 'FAILED')
                self.assertEqual(result['collection_gaps'][0]['attribution']['status'], 'UNATTRIBUTED')

    def test_final_or_unanchored_gap_is_not_attributed(self):
        for mode in ['final', 'unanchored', 'unknown-target', 'no-campaign']:
            with self.subTest(mode=mode):
                fixture = self.gap_campaign()
                if mode == 'final':
                    fixture[-1]['sample_kind'] = 'final'
                elif mode == 'unanchored':
                    fixture[-2].pop()
                elif mode == 'unknown-target':
                    fixture[-1]['target'] = None
                else:
                    fixture[0]['fault_campaign'] = False
                    for name in m.FAULT_CLASSES:
                        del fixture[3]['verdict']['classes'][name]
                    fixture[0]['turns_per_session'] = 2
                result = self.gap_summary(fixture)
                self.assertEqual(result['qualification']['status'], 'FAILED')
                self.assertEqual(result['collection_gaps'][0]['attribution']['status'], 'UNATTRIBUTED')

    def test_fault_window_inner_boundaries_and_restate_target_are_attributed(self):
        for instant in [52000, 108000]:
            with self.subTest(instant=instant):
                fixture = self.gap_campaign()
                fixture[-1]['monotonic_ns'] = instant
                fixture[-1]['target'] = {'component': 'restate', 'endpoint': 'admin'}
                for row in fixture[-3]:
                    row['kind'] = 'restate-restart'
                    row['fault_id'] = 'restate-restart'
                fixture[-3][0]['detail_json'] = json.dumps({'collection_targets': [fixture[-1]['target']]})
                result = self.gap_summary(fixture)
                self.assertEqual(result['qualification']['status'], 'PASSED')
                self.assertEqual(result['collection_gaps'][0]['attribution']['fault_id'], 'restate-restart')

    def test_archive_keeps_gap_attribution_and_unattributed_failure(self):
        for attributed, epochs in [(True, False), (False, False), (True, True), (False, True)]:
            with self.subTest(attributed=attributed, epochs=epochs), tempfile.TemporaryDirectory() as tmp:
                fixture = self.epoch_campaign() if epochs else self.gap_campaign()
                run, operations, samples, witness, faults, anchors, gap = fixture
                if not attributed:
                    gap['target']['endpoint'] = 'other-worker'
                    faults[0]['detail_json'] = json.dumps({'collection_targets': []})
                ledgers = witness_evidence(operations)
                next(row for row in ledgers if row['ledger'] == 'witness_load_faults')['rows'] = faults
                witness['fault_rows'] = len(faults)
                log = Path(tmp) / 'load.log'
                rows = [run, *operations, *samples, *anchors, gap, *ledgers, witness]
                log.write_text(''.join('load measurement ' + json.dumps(row) + '\n' for row in rows))
                with contextlib.redirect_stdout(io.StringIO()):
                    if attributed:
                        m.archive(log, Path(tmp) / 'results')
                    else:
                        reason = 'counter epoch gap' if epochs else 'load qualification is FAILED'
                        with self.assertRaisesRegex(ValueError, reason):
                            m.archive(log, Path(tmp) / 'results')
                output = Path(tmp) / 'results' / 'fig-3790' / 'r'
                saved = [json.loads(line) for line in (output / 'collection_gaps.jsonl').read_text().splitlines()]
                summary = json.loads((output / 'summary.json').read_text())
                self.assertEqual(summary['collection_gaps'], saved)
                self.assertEqual(len(saved), 5 if epochs else 1)
                self.assertTrue(all(row['attribution']['status'] == ('FAULT_ATTRIBUTED' if attributed else 'UNATTRIBUTED')
                                    for row in saved))
                if epochs and not attributed:
                    self.assertEqual(summary['verdict'], 'failed')
                else:
                    self.assertEqual(summary['qualification']['status'], 'PASSED' if attributed else 'FAILED')
                if epochs:
                    self.assertTrue(all(row['unobserved_delta'] is None and not row['complete']
                                        for row in saved if row['record'] == 'counter_gap'))
                self.assertEqual(json.loads((output / 'sample_errors.jsonl').read_text()), gap)

    def epoch_campaign(self):
        fixture = self.gap_campaign()
        samples = fixture[2]
        observed = copy.deepcopy(samples[-1])
        observed.update(monotonic_ns=80000, collection_finished_ns=85000)
        observed['workers'][0]['observed_ns'] = 81000
        for node in observed['restate']:
            node['metrics_observed_ns'] = 82000
        observed['workers'][0]['resources']['processes'][0]['epoch'] = '2'
        observed['workers'][0]['resources']['cgroup_cpu']['usage_usec'] = 3
        samples[-1]['workers'][0]['resources']['processes'][0]['epoch'] = '2'
        samples[-1]['workers'][0]['resources']['cgroup_cpu']['usage_usec'] = 5
        samples.insert(1, observed)
        return fixture

    def test_faulted_counter_epoch_gap_keeps_unknown_delta_and_values(self):
        result = self.gap_summary(self.epoch_campaign())
        self.assertEqual(result['qualification']['status'], 'PASSED')
        gaps = [row for row in result['collection_gaps'] if row['record'] == 'counter_gap']
        self.assertEqual(len(gaps), 4)
        gap = next(row for row in gaps if row['counter'] == 'worker_usage_usec')
        self.assertEqual(gap['before'], {'epoch': '1', 'value': 10})
        self.assertEqual(gap['after'], {'epoch': '2', 'value': 3})
        self.assertIsNone(gap['unobserved_delta'])
        self.assertFalse(gap['complete'])
        self.assertEqual(gap['attribution']['status'], 'FAULT_ATTRIBUTED')
        self.assertEqual(gap['attribution']['fault_id'], 'worker-kill')
        self.assertEqual(result['counters']['worker_usage_usec']['observed_delta'], 2)
        self.assertFalse(result['counters']['worker_usage_usec']['complete'])
        self.assertEqual(result['counters']['worker_usage_usec']['epoch_gaps'], 1)
        self.assertEqual(result['counters']['worker_usage_usec']['fault_attributed_epoch_gaps'], 1)

    def test_unfaulted_or_outside_counter_epoch_gap_raises_with_evidence(self):
        for outside in [True, False]:
            with self.subTest(outside=outside):
                fixture = self.epoch_campaign()
                if outside:
                    fixture[2][1].update(monotonic_ns=120000, collection_finished_ns=125000)
                    fixture[2][1]['workers'][0]['observed_ns'] = 121000
                    fixture[2][-1].update(monotonic_ns=130000, collection_finished_ns=135000)
                else:
                    fixture[-3][0]['detail_json'] = json.dumps({'collection_targets': [
                        {'component': 'worker', 'endpoint': 'other-worker'}]})
                with self.assertRaisesRegex(ValueError, 'counter epoch gap') as refused:
                    self.gap_summary(fixture)
                gaps = [row for row in refused.exception.collection_gaps if row['record'] == 'counter_gap']
                self.assertEqual(len(gaps), 4)
                self.assertTrue(all(row['attribution']['status'] == 'UNATTRIBUTED' for row in gaps))
                self.assertTrue(all(row['unobserved_delta'] is None and not row['complete'] for row in gaps))

    def test_campaign_recovery_is_complete_with_explicit_error_bars(self):
        run, operations, samples, witness = self.campaign_evidence()
        # Put the synthetic population on the same nanosecond scale as the anchors.
        for row in operations:
            for key in ['scheduled_ns', 'sent_ns', 'accepted_ns', 'observed_ns']:
                row[key] *= 1000
        for sample in samples:
            for key in ['monotonic_ns', 'collection_finished_ns']:
                sample[key] *= 1000
            for worker in sample['workers']:
                worker['observed_ns'] *= 1000
            for node in sample['restate']:
                node['metrics_observed_ns'] *= 1000
        rows, anchors = self.normalized_campaign()
        result = m.summarize(run, operations, samples, witness, faults=rows, anchors=anchors)
        self.assertEqual(result['qualification']['status'], 'PASSED')
        self.assertEqual(result['recovery_inputs']['status'], 'COMPLETE')
        self.assertEqual(result['recoveries'][0]['service_progress_bounds_ns'], [6000, 14000])
        self.assertEqual(result['recoveries'][0]['backlog_recovery_bounds_ns'], [56000, 64000])
        self.assertEqual(len(result['recovery_inputs']['normalized_rows']), len(rows))

    def test_foreign_or_duplicate_anchor_is_refused(self):
        rows, anchors = self.normalized_campaign()
        for key, value in [('run', 'other'), ('clock', 'host_wall_time')]:
            bad = copy.deepcopy(anchors)
            bad[1][key] = value
            with self.assertRaisesRegex(ValueError, 'mismatched anchor clock or run'):
                m.normalize_fault_rows(rows, bad, 'r')
        with self.assertRaisesRegex(ValueError, 'duplicate clock anchor'):
            m.normalize_fault_rows(rows, [*anchors, anchors[0]], 'r')

    def test_missing_recovery_timestamp_is_incomplete(self):
        rows, anchors = self.normalized_campaign()
        rows[-1]['detail_json'] = '{}'
        _, inputs, errors = m.recovery_inputs(rows, anchors, 'r', [], 'passed')
        self.assertEqual(inputs, [])
        self.assertIn('worker-kill/recovered/2: missing service_progress_at_us', errors)

    def test_recovery_refuses_unnormalized_clock(self):
        row = dict(schema_version=1, id='f', clock='witness_postgres_us')
        with self.assertRaisesRegex(ValueError, 'requires normalized driver-clock rows'):
            m.recoveries([row], [])

    def test_negative_offset_and_odd_round_trip_preserve_bounds(self):
        rows, anchors = self.normalized_campaign()
        rows[0]['recorded_at_us'] = 998
        anchors[1]['round_trip_ns'] = 2001
        normalized, errors = m.normalize_fault_rows(rows[:1], anchors, 'r')
        self.assertEqual(errors, [])
        self.assertEqual(normalized[0]['monotonic_ns'], 48000)
        self.assertEqual(normalized[0]['clock_bounds_ns'], [45999, 50001])
        anchors[1]['round_trip_ns'] = -1
        with self.assertRaisesRegex(ValueError, 'invalid clock anchor'):
            m.normalize_fault_rows(rows, anchors, 'r')

    def test_clock_step_outside_round_trip_bounds_stays_incomplete(self):
        rows, anchors = self.normalized_campaign()
        anchors[-1]['witness_us'] += 100
        normalized, errors = m.normalize_fault_rows(rows, anchors, 'r')
        self.assertEqual(len(normalized), 1)
        self.assertIn('worker-kill/recovered/2: witness clock offset moved beyond anchor bounds', errors)

    def test_archive_normalizes_its_independent_fault_ledger(self):
        run, operations, samples, witness = self.campaign_evidence()
        for row in operations:
            for key in ['scheduled_ns', 'sent_ns', 'accepted_ns', 'observed_ns']:
                row[key] *= 1000
        for sample in samples:
            for key in ['monotonic_ns', 'collection_finished_ns']:
                sample[key] *= 1000
            for worker in sample['workers']:
                worker['observed_ns'] *= 1000
            for node in sample['restate']:
                node['metrics_observed_ns'] *= 1000
        faults, anchors = self.normalized_campaign()
        witness['fault_rows'] = len(faults)
        ledgers = witness_evidence(operations)
        next(row for row in ledgers if row['ledger'] == 'witness_load_faults')['rows'] = faults
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / 'measurements.log'
            log.write_text(''.join('load measurement ' + json.dumps(row) + '\n'
                                   for row in [run, *operations, *samples, *anchors, *ledgers, witness]))
            with contextlib.redirect_stdout(io.StringIO()):
                m.archive(log, Path(tmp))
            output = Path(tmp) / 'fig-3790' / 'r'
            result = json.loads((output / 'summary.json').read_text())
            self.assertEqual(result['recovery_inputs']['status'], 'COMPLETE')
            self.assertEqual(result['faults_exercised'], 1)
            archived_faults = [json.loads(row) for row in (output / 'faults.jsonl').read_text().splitlines()]
            self.assertEqual(archived_faults, [dict(schema_version=1, **row) for row in faults])
            inputs = [json.loads(row) for row in (output / 'recovery_inputs.jsonl').read_text().splitlines()]
            self.assertEqual(inputs[0]['anchor_ids'], ['1', '2'])
            self.assertEqual(len((output / 'clock_anchors.jsonl').read_text().splitlines()), 3)

    def test_recovery_report_preserves_complete_bounds_when_other_measurements_fail(self):
        run, operations, _, witness = self.campaign_evidence()
        for row in operations:
            for key in ['scheduled_ns', 'sent_ns', 'accepted_ns', 'observed_ns']:
                row[key] *= 1000
        faults, anchors = self.normalized_campaign()
        witness['verdict']['classes']['turns']['violations'] = ['unrelated durability violation']
        result = m.recovery_report(run, operations, witness, faults, anchors)
        self.assertEqual(result['status'], 'COMPLETE')
        self.assertEqual(result['measurements'][0]['backlog_recovery_bounds_ns'], [56000, 64000])
        witness['verdict']['classes']['worker-kill']['violations'] = ['lost answer']
        self.assertEqual(m.recovery_report(run, operations, witness, faults, anchors)['status'], 'INCOMPLETE')

    def test_recovery_archive_precedes_and_does_not_pass_the_final_census(self):
        run, operations, _, witness = self.campaign_evidence()
        for row in operations:
            for key in ['scheduled_ns', 'sent_ns', 'accepted_ns', 'observed_ns']:
                row[key] *= 1000
        faults, anchors = self.normalized_campaign()
        witness['fault_rows'] = len(faults)
        ledgers = witness_evidence(operations)
        next(row for row in ledgers if row['ledger'] == 'witness_load_faults')['rows'] = faults
        with tempfile.TemporaryDirectory() as tmp:
            log = Path(tmp) / 'measurements.log'
            # There is no final sample, and the recovery still has all its witnesses.
            log.write_text(''.join('load measurement ' + json.dumps(row) + '\n'
                                   for row in [run, *operations, *anchors, *ledgers, witness]))
            with contextlib.redirect_stdout(io.StringIO()):
                m.archive(log, Path(tmp), recovery_only=True)
            output = Path(tmp) / 'fig-3790' / 'r'
            self.assertEqual(json.loads((output / 'recovery.json').read_text())['status'], 'COMPLETE')
            self.assertEqual(json.loads((output / 'summary.json').read_text())['verdict'], 'INCOMPLETE')
            self.assertEqual(len((output / 'normalized_faults.jsonl').read_text().splitlines()), 2)
            with self.assertRaisesRegex(ValueError, 'missing initial/final metric sample'):
                with contextlib.redirect_stdout(io.StringIO()):
                    m.archive(log, Path(tmp))
            self.assertEqual(json.loads((output / 'summary.json').read_text())['verdict'], 'failed')
            self.assertEqual(json.loads((output / 'recovery.json').read_text())['status'], 'COMPLETE')

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
