#!/usr/bin/env python3
"""The repro capture of a failed load run (FIG-4264).

`multi-node-load.sh` runs this only when a load run has failed, before it
deletes the topology, so a green run pays nothing for it. It archives what a
runtime diagnosis needs and the recipe would otherwise destroy, under
`<run>/raw-journals/<utc>/`:

- both witness ledgers (`witness_load_events`, `witness_load_faults`);
- every outer `E2eLoadWorkflow` invocation and every invocation not yet
  completed, with their journals;
- for each affected session, its `LashSession` and `LashTurn` invocations
  (completed and open), the open ones apart, and all of their journals.

An affected session is one whose operation a witness violation, a driver
diagnosis or a failed fault names. The witness events map an operation's
`run/actor/ordinal` subject to the session it was sent to. A `LashTurn` key is
`{len}:{session}{root}` (`lash_restate::turn_workflow_key`), so a session's
turns are the keys that start with `{len}:{session}`.

Restate is read through its admin SQL endpoint and the witness through psql,
both from the chart's fault probe pod, as the fault controller does.
"""

from __future__ import annotations

import argparse
import datetime
import json
import re
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Callable, Iterable

JOURNAL_GROUP = 20
CAPTURE_ATTEMPTS = 8
CAPTURE_RETRY_S = 15


# -- pure decisions ----------------------------------------------------------


def failure_lines(load_log: str, faults_log: str) -> list[str]:
    """The lines that name a failed operation: the witness's violations, the
    driver's diagnoses and the fault controller's failures."""
    lines = [line for line in load_log.splitlines()
             if 'load witness violation ' in line or 'load diagnosis key=' in line]
    lines += [line for line in faults_log.splitlines() if 'fail' in line.lower()]
    return lines


def failed_subjects(run: str, lines: Iterable[str]) -> list[str]:
    """The `run/actor/ordinal` subjects of the operations `lines` name, in
    first-mention order. Nested keys (`…/queued/0`, `…/cancel`) name their
    operation's subject."""
    pattern = re.compile(re.escape(run) + r'/(\d+)/(\d+)')
    subjects: dict[str, None] = {}
    for line in lines:
        for actor, ordinal in pattern.findall(line):
            subjects[f'{run}/{actor}/{ordinal}'] = None
    return list(subjects)


def event_session(event: dict[str, Any]) -> str | None:
    detail = event.get('detail_json')
    if isinstance(detail, str):
        detail = json.loads(detail)
    if not isinstance(detail, dict):
        return None
    request = detail.get('request')
    if isinstance(request, dict) and isinstance(request.get('session_id'), str):
        return request['session_id']
    session = detail.get('session_id')
    return session if isinstance(session, str) else None


def affected_sessions(subjects: Iterable[str], events: Iterable[dict[str, Any]]) -> list[str]:
    """The sessions the witness events record for `subjects`, in subject
    order. An event belongs to a subject when its own subject is the subject
    or nests under it."""
    events = list(events)
    sessions: dict[str, None] = {}
    for subject in subjects:
        for event in events:
            own = event.get('subject', '')
            if own == subject or own.startswith(subject + '/'):
                session = event_session(event)
                if session:
                    sessions[session] = None
    return list(sessions)


def turn_key_prefix(session: str) -> str:
    """The prefix every `LashTurn` key of `session` starts with."""
    return f'{len(session.encode())}:{session}'


def literal(value: str) -> str:
    return "'" + value.replace("'", "''") + "'"


def session_invocations_sql(session: str) -> str:
    return ('SELECT * FROM sys_invocation WHERE '
            "((target_service_name = 'LashTurn' OR target_service_name LIKE 'LashTurn_g%') "
            f'AND starts_with(target_service_key, {literal(turn_key_prefix(session))})) OR '
            "((target_service_name = 'LashSession' OR target_service_name LIKE 'LashSession_g%') "
            f'AND target_service_key = {literal(session)})')


def journal_sql(ids: list[str]) -> str:
    return 'SELECT * FROM sys_journal WHERE id IN (' + ','.join(literal(i) for i in ids) + ') ORDER BY id, index'


def groups(values: list[str], size: int = JOURNAL_GROUP) -> list[list[str]]:
    return [values[index:index + size] for index in range(0, len(values), size)]


# -- the capture -------------------------------------------------------------


Exec = Callable[[list[str], str], str]


class Capture:
    """One capture into `out`. `exec_probe(argv, stdin)` runs `argv` in the
    fault probe pod and returns its stdout, raising on failure."""

    def __init__(self, out: Path, name: str, exec_probe: Exec):
        self.out, self.name, self.exec_probe = out, name, exec_probe

    def command(self, label: str, argv: list[str], stdin: str) -> str:
        last: Exception | None = None
        for attempt in range(3):
            try:
                return self.exec_probe(argv, stdin)
            except Exception as error:  # noqa: BLE001 - every failure is retried, then reported
                last = error
                (self.out / f'{label}.{attempt}.error').write_text(str(error))
        raise RuntimeError(f'{label}: {last}')

    def query(self, label: str, sql: str) -> list[dict[str, Any]]:
        (self.out / f'{label}.sql').write_text(sql)
        text = self.command(label, ['curl', '--max-time', '45', '-fsS', '-X', 'POST',
                                    '-H', 'content-type: application/json',
                                    '-H', 'accept: application/json', '--data-binary', '@-',
                                    f'http://{self.name}-restate:9070/query'],
                            json.dumps({'query': sql}))
        rows = json.loads(text)['rows']
        (self.out / f'{label}.json').write_text(json.dumps({'rows': rows}))
        return rows

    def witness(self, table: str) -> list[dict[str, Any]]:
        text = self.command(table, ['bash', '-c', 'exec psql "$WITNESS_DATABASE_URL" -X -q -A -t '
                                    '-v ON_ERROR_STOP=1'], f'SELECT row_to_json(t) FROM {table} t;')
        (self.out / f'{table}.jsonl').write_text(text)
        return [json.loads(line) for line in text.splitlines() if line.strip()]

    def journals(self, label: str, ids: list[str]) -> int:
        return sum(len(self.query(f'{label}-{index}', journal_sql(group)))
                   for index, group in enumerate(groups(ids)))

    def run(self, run: str, failure: list[str]) -> dict[str, Any]:
        events = self.witness('witness_load_events')
        self.witness('witness_load_faults')
        outer = self.query('sys_invocation',
                           "SELECT * FROM sys_invocation WHERE target_service_name = 'E2eLoadWorkflow'")
        open_rows = self.query('sys_invocation_open',
                               "SELECT * FROM sys_invocation WHERE status <> 'completed'")
        outer_ids = list(dict.fromkeys(row['id'] for row in outer + open_rows))
        journal_rows = self.journals('sys_journal', outer_ids)
        subjects = failed_subjects(run, failure)
        sessions = []
        for index, session in enumerate(affected_sessions(subjects, events)):
            label = f'session-{index}'
            rows = self.query(f'{label}-invocations', session_invocations_sql(session))
            still_open = [row for row in rows if row.get('status') != 'completed']
            (self.out / f'{label}-open.json').write_text(json.dumps({'rows': still_open}))
            ids = [row['id'] for row in rows]
            sessions.append({'session': session, 'label': label,
                             'turn_key_prefix': turn_key_prefix(session),
                             'turns': sum(1 for row in rows
                                          if str(row.get('target_service_name', '')).startswith('LashTurn')),
                             'invocation_ids': ids,
                             'open_invocation_ids': [row['id'] for row in still_open],
                             'journal_rows': self.journals(f'{label}-journal', ids)})
        summary = {'captured_utc': datetime.datetime.now(datetime.timezone.utc).isoformat(),
                   'run': run, 'failed_subjects': subjects, 'invocation_ids': outer_ids,
                   'journal_rows': journal_rows, 'sessions': sessions}
        (self.out / 'complete.json').write_text(json.dumps(summary, indent=1))
        return summary


def kubectl_probe(kubeconfig: str, namespace: str, name: str) -> Exec:
    prefix = ['kubectl', '--kubeconfig', kubeconfig, '--namespace', namespace,
              'exec', '-i', f'deployment/{name}-fault-probe', '--']

    def run(argv: list[str], stdin: str) -> str:
        result = subprocess.run(prefix + argv, input=stdin, capture_output=True, text=True, timeout=60)
        if result.returncode != 0:
            raise RuntimeError(f'exit {result.returncode}: {result.stderr.strip()}')
        return result.stdout
    return run


def read(path: Path) -> str:
    return path.read_text(errors='replace') if path.exists() else ''


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.split('\n\n')[0])
    parser.add_argument('--run-dir', required=True)
    parser.add_argument('--kubeconfig', required=True)
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--name', required=True)
    args = parser.parse_args()
    run_dir = Path(args.run_dir)
    import yaml
    run = yaml.safe_load((run_dir / 'load-values.yaml').read_text())['load']['run']
    # The recipe saves the load Job's log only once the driver ends; the
    # cleanup's prefixed pod log covers a driver that never got there.
    load_log = read(run_dir / 'load.log') or '\n'.join(read(path) for path in sorted(run_dir.glob('*-load-*.log')))
    failure = failure_lines(load_log, read(run_dir / 'faults.log'))
    exec_probe = kubectl_probe(args.kubeconfig, args.namespace, args.name)
    for attempt in range(CAPTURE_ATTEMPTS):
        out = run_dir / 'raw-journals' / datetime.datetime.now(datetime.timezone.utc).strftime('%Y%m%dT%H%M%S.%fZ')
        out.mkdir(parents=True)
        try:
            summary = Capture(out, args.name, exec_probe).run(run, failure)
        except Exception as error:  # noqa: BLE001 - a partial capture is kept and retried
            (out / 'incomplete.txt').write_text(str(error))
            print(f'repro capture attempt {attempt + 1} failed: {error}', file=sys.stderr, flush=True)
            time.sleep(CAPTURE_RETRY_S)
            continue
        print(f"repro capture: {out} sessions={len(summary['sessions'])} "
              f"failed_subjects={len(summary['failed_subjects'])}", flush=True)
        return 0
    return 1


if __name__ == '__main__':
    sys.exit(main())
