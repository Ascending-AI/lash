#!/usr/bin/env python3
"""The repro capture of a failed load run (FIG-4264), over the recorded shape
of FIG-4457's campaign: a queued input cancelled by its sibling's cancel."""
import json
import tempfile
import unittest
from pathlib import Path

import loadtest_repro as repro

RUN = 'smoke-v1-20260930222917'
SESSION = f'load-{RUN}-1-g7'
OTHER = f'load-{RUN}-0-g3'

LOAD_LOG = f"""load turn operation={RUN}/1/17 status=Answered credits_input=true root=load-{RUN}-1-17
load witness class=queued-inputs witnessed=21 violations=1
load witness violation class=queued-inputs: queued input `{RUN}/1/18/queued/0` ended Cancelled under root Some("load-{RUN}-1-18-queued-0") with final value null and provider receipts Some(["load_queued"])
load witness verdict=failed run={RUN} classes=31 covered=31 violations=1 absorbed_effect_attempts=1
load diagnosis key={RUN}/1/18/queued/0 receipts=1 messages=12 first_failure= tail="user": `op` = \\"{RUN}/1/19\\"
"""


def event(subject, detail):
    return {'subject': subject, 'operation': 'turn', 'phase': 'sent', 'detail_json': json.dumps(detail)}


EVENTS = [
    event(f'{RUN}/0/20', {'request': {'session_id': OTHER}}),
    event(f'{RUN}/1/18', {'request': {'session_id': SESSION}}),
    event(f'{RUN}/1/18/blob/0', {'session_id': SESSION}),
    event(f'{RUN}/1/19', {'request': {'session_id': SESSION}}),
    event(f'{RUN}/1/180', {'request': {'session_id': 'load-unrelated'}}),
]


def invocation(id, service, key, status='completed'):
    return {'id': id, 'target_service_name': service, 'target_service_key': key, 'status': status}


class FakeProbe:
    """The fault probe pod: psql over the witness, and Restate's admin SQL."""

    def __init__(self):
        self.sql = []

    def __call__(self, argv, stdin):
        if argv[0] == 'bash':
            table = stdin.split(' FROM ')[1].split()[0]
            rows = EVENTS if table == 'witness_load_events' else []
            return ''.join(json.dumps(row) + '\n' for row in rows)
        query = json.loads(stdin)['query']
        self.sql.append(query)
        if 'sys_journal' in query:
            ids = [part.strip("'") for part in query.split('IN (')[1].split(')')[0].split(',')]
            return json.dumps({'rows': [{'id': id, 'index': 0, 'entry_type': 'Command: Run'} for id in ids]})
        if "target_service_name = 'E2eLoadWorkflow'" in query:
            return json.dumps({'rows': [invocation('inv_outer', 'E2eLoadWorkflow', f'{RUN}/1/18')]})
        if "status <> 'completed'" in query:
            return json.dumps({'rows': [invocation('inv_open', 'LashProcessWorkflow', 'p_1', 'suspended')]})
        if 'LashTurn' in query:
            prefix = repro.turn_key_prefix(SESSION)
            assert f"starts_with(target_service_key, '{prefix}')" in query, query
            return json.dumps({'rows': [
                invocation('inv_s', 'LashSession', SESSION),
                invocation('inv_t17', 'LashTurn', f'{prefix}load-{RUN}-1-17'),
                invocation('inv_t18q', 'LashTurn', f'{prefix}load-{RUN}-1-18-queued-0', 'suspended'),
            ]})
        raise AssertionError(query)


class AffectedSessionTest(unittest.TestCase):
    def test_a_violation_and_its_diagnosis_name_their_operations_subjects(self):
        lines = repro.failure_lines(LOAD_LOG, '')
        self.assertEqual(len(lines), 2)
        self.assertEqual(repro.failed_subjects(RUN, lines), [f'{RUN}/1/18', f'{RUN}/1/19'])

    def test_a_green_log_names_no_operation(self):
        green = '\n'.join(line for line in LOAD_LOG.splitlines() if 'violation' not in line and 'diagnosis' not in line)
        self.assertEqual(repro.failed_subjects(RUN, repro.failure_lines(green, 'fault worker-kill recovered')), [])

    def test_a_failed_fault_names_its_operation(self):
        lines = repro.failure_lines('', f'FaultFailed: {RUN}/0/20 never answered\nfault restate-restart recovered\n')
        self.assertEqual(repro.failed_subjects(RUN, lines), [f'{RUN}/0/20'])

    def test_a_subject_resolves_to_the_session_its_events_record_and_not_a_longer_ordinal(self):
        sessions = repro.affected_sessions([f'{RUN}/1/18'], EVENTS)
        self.assertEqual(sessions, [SESSION])

    def test_the_turn_key_prefix_is_the_session_length_and_id(self):
        self.assertEqual(repro.turn_key_prefix('s-1'), '3:s-1')
        self.assertEqual(repro.turn_key_prefix('é'), '2:é')


class CaptureTest(unittest.TestCase):
    def test_a_failed_run_captures_the_affected_sessions_turn_journals_and_open_invocations(self):
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            probe = FakeProbe()
            summary = repro.Capture(out, 'lash-loadtest', probe).run(RUN, repro.failure_lines(LOAD_LOG, ''))
            self.assertEqual([session['session'] for session in summary['sessions']], [SESSION])
            session = summary['sessions'][0]
            self.assertEqual(session['turns'], 2)
            self.assertEqual(session['open_invocation_ids'], ['inv_t18q'])
            self.assertEqual(session['journal_rows'], 3)
            for name in ['witness_load_events.jsonl', 'witness_load_faults.jsonl', 'sys_invocation.json',
                         'sys_invocation_open.json', 'sys_journal-0.json', 'session-0-invocations.sql',
                         'session-0-invocations.json', 'session-0-open.json', 'session-0-journal-0.json',
                         'complete.json']:
                self.assertTrue((out / name).is_file(), name)
            opened = json.loads((out / 'session-0-open.json').read_text())['rows']
            self.assertEqual([row['id'] for row in opened], ['inv_t18q'])
            journal = json.loads((out / 'session-0-journal-0.json').read_text())['rows']
            self.assertEqual(sorted(row['id'] for row in journal), ['inv_s', 'inv_t17', 'inv_t18q'])
            self.assertEqual(json.loads((out / 'complete.json').read_text())['sessions'][0]['session'], SESSION)

    def test_a_probe_failure_is_retried_then_reported(self):
        def broken(argv, stdin):
            raise RuntimeError('connection refused')
        with tempfile.TemporaryDirectory() as directory:
            out = Path(directory)
            with self.assertRaises(RuntimeError):
                repro.Capture(out, 'lash-loadtest', broken).run(RUN, [])
            self.assertEqual(len(list(out.glob('witness_load_events.*.error'))), 3)
            self.assertFalse((out / 'complete.json').exists())

    def test_journals_are_read_in_bounded_groups(self):
        ids = [f'inv_{index}' for index in range(45)]
        self.assertEqual([len(group) for group in repro.groups(ids)], [20, 20, 5])
        self.assertIn("'inv_0','inv_1'", repro.journal_sql(ids[:2]))


if __name__ == '__main__':
    unittest.main()
