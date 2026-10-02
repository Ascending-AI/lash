#!/usr/bin/env python3
"""The FIG-3790 fault controller (lane L4, FIG-4169).

Runs one fault campaign against a live load run: a SIGKILL of a worker that
is running load work, a restart of a Restate node leading partitions that are
processing work, and a rolling deploy of a replacement worker generation with
a generation drain. Fault times come from the workload's `faults` section,
relative to the fault phase, which starts with the driver's first send.

Every fault is recorded in the witness ledger (`witness_load_faults`) on the
witness database's clock: the controller's intent, the injection with the busy
work it hit, and the recovery it waited for. A fault that finds no busy target
within the watchdog is invalid and fails the campaign; so does one that does
not recover. The driver keeps its sessions sending until the campaign row
`complete` (or `failed`), then reconciles the ledgers, fault classes included.

"Recovered" is: every operation in flight at the injection has its terminal
and it is an answer, a turn sent after the injection answered, a queued input
answered, a cron emission started its process, the backlog returned to its
pre-fault range, and the fault's own target recovered (the same worker pod
restarted after its hold, the same Restate node identity rejoined with every
partition led, or the old generation drained with no pinned invocation left
before its retirement). Throughput and latency around each fault are recorded,
not gated: budgets come from the later baseline.

The controller reaches the cluster only through kubectl: Restate's cluster
tables through `restatectl sql` in a Restate pod, and the witness, Restate's
admin API and the workers' control endpoints from the chart's fault probe pod.
"""

from __future__ import annotations

import argparse
import dataclasses
import json
import shlex
import subprocess
import sys
import time
from pathlib import Path
from typing import Any, Callable
from loadtest_ledger import require_pair

# How long a fault may wait for a busy target, and for its recovery. The
# recovery watchdog is the workload's drain timeout: a test timeout, not a
# release budget.
BUSY_WATCHDOG_S = 120
POLL_S = 1.0
WORKER_PROCESS = 'lash-e2e-worker'
RESTATE_PROCESS = 'restate-server'


class FaultFailed(Exception):
    """A fault that missed active work, or did not recover."""


# -- pure decisions ----------------------------------------------------------


def restatectl_rows(output: str) -> list[dict[str, Any]]:
    """The rows of `restatectl sql --json`, which may print a row count first."""
    start = output.find('[')
    if start < 0:
        raise ValueError(f'restatectl printed no JSON rows: {output!r}')
    rows = json.loads(output[start:])
    if not isinstance(rows, list):
        raise ValueError('restatectl rows are not a list')
    return rows


def leaders(partition_rows: list[dict[str, Any]]) -> dict[int, dict[str, Any]]:
    """Each partition's leader row, by partition id."""
    led: dict[int, dict[str, Any]] = {}
    for row in partition_rows:
        if row['effective_mode'] == 'Leader':
            partition = int(row['partition_id'])
            if partition in led:
                raise ValueError(f'partition {partition} reports two leaders')
            led[partition] = row
    return led


@dataclasses.dataclass(frozen=True)
class RestateTarget:
    node: str
    generation: str
    pod: str
    led: tuple[int, ...]
    advancing: tuple[int, ...]
    epochs: dict[int, int]


def choose_restate_target(before: list[dict[str, Any]], after: list[dict[str, Any]],
                          nodes: list[dict[str, Any]]) -> RestateTarget | None:
    """The alive node leading the most partitions whose applied log advanced
    between two samples: a leader that is processing work right now."""
    first, second = leaders(before), leaders(after)
    names = {row['plain_node_id']: row for row in nodes if row['state'] == 'alive'}
    best: RestateTarget | None = None
    for node, row in names.items():
        led = sorted(partition for partition, leader in second.items() if leader['plain_node_id'] == node)
        advancing = [
            partition for partition in led
            if partition in first
            and first[partition]['plain_node_id'] == node
            and int(second[partition]['applied_log_lsn']) > int(first[partition]['applied_log_lsn'])
        ]
        if not advancing:
            continue
        candidate = RestateTarget(
            node=node, generation=row['gen_node_id'], pod=row['name'], led=tuple(led),
            advancing=tuple(advancing),
            epochs={partition: int(second[partition]['leader_epoch']) for partition in led},
        )
        if best is None or len(candidate.advancing) > len(best.advancing):
            best = candidate
    return best


def restate_recovery(target: RestateTarget, partitions: int, partition_rows: list[dict[str, Any]],
                     nodes: list[dict[str, Any]]) -> dict[str, Any] | None:
    """The restart's recovery evidence once the node rejoined under its
    identity with a new generation, every node is alive, every partition has
    a leader, and every partition it led was re-elected; else `None`."""
    by_node = {row['plain_node_id']: row for row in nodes}
    restarted = by_node.get(target.node)
    if restarted is None or restarted['name'] != target.pod:
        return None
    if any(row['state'] != 'alive' for row in nodes) or restarted['gen_node_id'] == target.generation:
        return None
    led = leaders(partition_rows)
    if len(led) != partitions:
        return None
    bumped = [partition for partition in target.led
              if partition in led and int(led[partition]['leader_epoch']) > target.epochs[partition]]
    if len(bumped) != len(target.led):
        return None
    return {
        'same_node_id': True, 'node': target.node, 'pod': target.pod,
        'generation_after': restarted['gen_node_id'], 'leaders': len(led), 'partitions': partitions,
        'epochs_bumped': len(bumped),
        'leaders_on_restarted': sum(1 for row in led.values() if row['plain_node_id'] == target.node),
    }


def choose_worker(activities: list[dict[str, Any]]) -> dict[str, Any] | None:
    """The worker running the most load operations, if any runs one."""
    busy = [activity for activity in activities if activity['active']]
    return max(busy, key=lambda activity: len(activity['active'])) if busy else None


def container_status(pod: dict[str, Any], container: str) -> dict[str, Any]:
    for status in pod['status'].get('containerStatuses', []):
        if status['name'] == container:
            return status
    raise ValueError(f"pod {pod['metadata']['name']} has no container {container}")


def pod_restart(before: dict[str, Any], after: dict[str, Any], container: str,
                exit_codes: tuple[int, ...]) -> dict[str, Any] | None:
    """The same pod's container restarted and is ready again; else `None`.
    The restart is the restart count, which the kubelet always reports. The
    stopped container's exit is in `lastState` only while the runtime still
    holds that container: a reported exit must be one of `exit_codes`, and an
    unreported one is recorded as `None`. The caller's restart hold ties the
    restart to its fault."""
    if after['metadata']['uid'] != before['metadata']['uid']:
        return {'same_pod': False}
    was, now = container_status(before, container), container_status(after, container)
    if now['restartCount'] <= was['restartCount'] or not now.get('ready'):
        return None
    terminated = now.get('lastState', {}).get('terminated')
    if terminated and terminated['exitCode'] not in exit_codes:
        raise FaultFailed(f"{container} stopped with exit code {terminated['exitCode']}, expected {exit_codes}")
    return {
        'same_pod': True, 'pod': after['metadata']['name'], 'pod_ip': after['status'].get('podIP'),
        'restarts_before': was['restartCount'], 'restarts_after': now['restartCount'],
        'exit_code': terminated['exitCode'] if terminated else None,
    }


@dataclasses.dataclass
class Backlog:
    """Open operations (sent, no terminal) sampled in every healthy window
    after the warm-up: the pre-fault range a fault's backlog must return to.
    An open-loop load's backlog is noisy at smoke scale, so the range is the
    whole healthy history, never one fault's preceding seconds."""
    healthy: list[int] = dataclasses.field(default_factory=list)

    def sample(self, value: int) -> None:
        self.healthy.append(value)

    def pre_fault_max(self) -> int:
        if not self.healthy:
            raise FaultFailed('no healthy backlog sample precedes the fault')
        return max(self.healthy)


def recovered(state: dict[str, Any]) -> bool:
    """Whether the witness shows service recovered from a fault."""
    return (state['in_flight_unfinished'] == 0 and state['turn'] and state['queued']
            and state['cron'] and state['moved'] and state['backlog'] <= state['pre_fault_max']
            and state['target'] is not None)


# -- the cluster --------------------------------------------------------------


class Cluster:
    def __init__(self, kubeconfig: str, namespace: str, log: Path):
        self.kubeconfig, self.namespace, self.log = kubeconfig, namespace, log

    def run(self, argv: list[str], *, stdin: str | None = None, check: bool = True,
            timeout: float = 120) -> subprocess.CompletedProcess[str]:
        with self.log.open('a') as log:
            log.write(f'$ {shlex.join(argv)}\n')
        result = subprocess.run(argv, input=stdin, capture_output=True, text=True, timeout=timeout)
        with self.log.open('a') as log:
            log.write(f'exit={result.returncode}\n{result.stdout[-4000:]}{result.stderr[-4000:]}\n')
        if check and result.returncode != 0:
            raise RuntimeError(f'{shlex.join(argv[:6])}... exited {result.returncode}: {result.stderr.strip()}')
        return result

    def kubectl(self, *args: str, stdin: str | None = None, check: bool = True,
                timeout: float = 120) -> subprocess.CompletedProcess[str]:
        return self.run(['kubectl', '--kubeconfig', self.kubeconfig, '--namespace', self.namespace, *args],
                        stdin=stdin, check=check, timeout=timeout)

    def json(self, *args: str) -> dict[str, Any]:
        return json.loads(self.kubectl('get', *args, '-o', 'json').stdout)


class Campaign:
    def __init__(self, args: argparse.Namespace):
        self.args = args
        self.run_dir = Path(args.run_dir)
        self.cluster = Cluster(args.kubeconfig, args.namespace, self.run_dir / 'faults-kubectl.log')
        self.targets = json.loads(self.cluster.json('configmap', f'{args.name}-faults')['data']['targets.json'])
        self.name = self.targets['name']
        self.workload = json.loads(Path(args.workload).read_text())
        self.faults = self.workload['faults']
        self.settle_s = int(self.workload['drain_timeout_s'])
        self.stable_s = int(self.workload['collection']['recovery_stable_s'])
        self.backlog = Backlog()
        self.ledger = self.run_dir / 'faults.jsonl'

    # -- in-cluster hands --

    def probe(self, script: str, *argv: str, stdin: str | None = None, check: bool = True
              ) -> subprocess.CompletedProcess[str]:
        return self.cluster.kubectl('exec', '-i', f"deployment/{self.targets['probe']}", '--',
                                    'bash', '-ec', script, 'probe', *argv, stdin=stdin, check=check)

    def http(self, method: str, url: str, body: Any = None) -> Any:
        result = self.probe('curl -fsS -X "$1" -H "content-type: application/json" '
                            '-H "accept: application/json" --data-binary @- "$2"',
                            method, url, stdin='' if body is None else json.dumps(body))
        return json.loads(result.stdout) if result.stdout.strip() else None

    def sql(self, query: str, **variables: Any) -> list[list[str]]:
        """Rows of a witness query, as tab-separated text fields."""
        flags = [item for key, value in variables.items() for item in ('-v', f'{key}={value}')]
        result = self.probe('exec psql "$WITNESS_DATABASE_URL" -X -q -A -t -F "$(printf "\\t")" '
                            '-v ON_ERROR_STOP=1 "$@"', *flags, stdin=query)
        return [line.split('\t') for line in result.stdout.splitlines() if line]

    def restate(self, query: str, avoid: str | None = None) -> list[dict[str, Any]]:
        pods = [pod for pod in self.targets['restatePods'] if pod != avoid]
        last = None
        for pod in pods:
            result = self.cluster.kubectl('exec', pod, '-c', 'restate', '--', 'restatectl', 'sql',
                                          '--json', query, check=False)
            if result.returncode == 0:
                return restatectl_rows(result.stdout)
            last = result.stderr
        raise RuntimeError(f'no Restate node answered `{query}`: {last}')

    def restart_in_place(self, pod: str, container: str, process: str, signal: str,
                         kind: str, delay_s: int) -> None:
        """Leave the restart hold, then signal the container's main process.
        The exec dies with the container, so its exit status is not checked:
        the pod's container status is the evidence."""
        script = ('printf "%s %s\\n" "$1" "$2" > /fault/restart-hold; '
                  'for entry in /proc/[0-9]*; do '
                  'if [ "$(cat "$entry/comm" 2>/dev/null)" = "$3" ]; then kill -"$4" "${entry#/proc/}"; fi; '
                  'done')
        self.cluster.kubectl('exec', pod, '-c', container, '--', 'sh', '-c', script, 'fault',
                             kind, str(delay_s), process, signal, check=False)

    def held(self, pod: str, container: str, kind: str) -> bool:
        return self.cluster.kubectl('exec', pod, '-c', container, '--', 'test', '-e',
                                    f'/fault/held-{kind}', check=False).returncode == 0

    # -- the witness --

    def record(self, kind: str, phase: str, target: str, detail: dict[str, Any]) -> int:
        require_pair('faults', kind, phase)
        rows = self.sql(
            "INSERT INTO witness_load_faults (run_id, kind, phase, target, detail_json) "
            "VALUES (:'run', :'kind', :'phase', :'target', :'detail') RETURNING recorded_at_us",
            run=self.args.run, kind=kind, phase=phase, target=target,
            detail=json.dumps(detail, sort_keys=True))
        at = int(rows[0][0])
        row = {'kind': kind, 'phase': phase, 'target': target,
               'recorded_at_us': at, 'detail': detail}
        with self.ledger.open('a') as ledger:
            ledger.write(json.dumps(row, sort_keys=True) + '\n')
        print(f'fault {kind} {phase} target={target} at_us={at} {json.dumps(detail, sort_keys=True)}', flush=True)
        return at

    def clock_us(self) -> int:
        return int(self.sql('SELECT witness_clock_us()')[0][0])

    OPERATIONS = """
        WITH sent AS (
            SELECT operation, subject, min(recorded_at_us) AS at FROM witness_load_events
            WHERE run_id = :'run' AND phase = 'sent' GROUP BY operation, subject
        ), ended AS (
            SELECT DISTINCT ON (operation, subject) operation, subject, recorded_at_us AS at,
                   detail_json::jsonb -> 'response' AS response
            FROM witness_load_events WHERE run_id = :'run' AND phase = 'terminal'
            ORDER BY operation, subject, event_id DESC
        )
    """

    def backlog_now(self) -> int:
        return int(self.sql(self.OPERATIONS + """
            SELECT count(*) FROM sent LEFT JOIN ended USING (operation, subject) WHERE ended.at IS NULL
        """, run=self.args.run)[0][0])

    def in_flight(self, at: int) -> list[dict[str, Any]]:
        rows = self.sql(self.OPERATIONS + """
            SELECT sent.operation, sent.subject, coalesce(ended.at::text, ''),
                   (ended.response IS NOT NULL)::text
            FROM sent LEFT JOIN ended USING (operation, subject)
            WHERE sent.at < :at AND (ended.at IS NULL OR ended.at > :at)
        """, run=self.args.run, at=at)
        return [{'operation': operation, 'subject': subject, 'ended_at': int(ended) if ended else None,
                 'answered': answered == 'true'} for operation, subject, ended, answered in rows]

    def progress(self, at: int, workers: list[str] | None = None) -> dict[str, Any]:
        """What service did after `at`: the first answered turn, queued input
        and cron emission of operations sent after it, on the witness clock."""
        (turn, queued, cron, moved), = self.sql(self.OPERATIONS + """
            SELECT
              coalesce(min(ended.at) FILTER (WHERE sent.operation = 'turn'
                AND ended.response -> 'outcome' ->> 'status' = 'answered')::text, ''),
              coalesce(min(ended.at) FILTER (WHERE sent.operation = 'turn'
                AND EXISTS (SELECT 1 FROM jsonb_array_elements(ended.response -> 'queued') AS input
                            WHERE input -> 'outcome' ->> 'status' = 'answered'))::text, ''),
              coalesce(min(ended.at) FILTER (WHERE sent.operation = 'cron-tick'
                AND jsonb_array_length(ended.response -> 'started_process_ids') > 0)::text, ''),
              coalesce(min(ended.at) FILTER (WHERE sent.operation = 'turn'
                AND ended.response -> 'outcome' ->> 'status' = 'answered'
                AND ended.response ->> 'worker_id' = ANY (string_to_array(:'workers', ',')))::text, '')
            FROM sent JOIN ended USING (operation, subject)
            WHERE sent.at > :at
        """, run=self.args.run, at=at, workers=','.join(workers or []))
        seconds = lambda value: (int(value) - at) / 1e6 if value else None
        return {'turn': seconds(turn), 'queued': seconds(queued), 'cron': seconds(cron),
                'moved': seconds(moved)}

    def window(self, start: int, end: int) -> dict[str, Any]:
        """Completed operations and their send-to-terminal latency in a window
        of the witness clock: recorded, not gated, until budgets exist."""
        (count, p50, p95), = self.sql(self.OPERATIONS + """
            SELECT count(*),
                   coalesce(percentile_cont(0.5) WITHIN GROUP (ORDER BY ended.at - sent.at)::text, ''),
                   coalesce(percentile_cont(0.95) WITHIN GROUP (ORDER BY ended.at - sent.at)::text, '')
            FROM sent JOIN ended USING (operation, subject)
            WHERE ended.at >= :start AND ended.at < :end AND ended.response IS NOT NULL
        """, run=self.args.run, start=start, end=end)
        span = max(end - start, 1) / 1e6
        return {'seconds': round(span, 3), 'completed': int(count),
                'completed_per_s': round(int(count) / span, 3),
                'p50_ms': round(float(p50) / 1e3, 1) if p50 else None,
                'p95_ms': round(float(p95) / 1e3, 1) if p95 else None}

    # -- the campaign --

    def wait_until(self, due_us: int, steady_from_us: int) -> None:
        """Sample the healthy backlog from the end of the warm-up until the
        witness clock reaches `due_us`."""
        while True:
            now = self.clock_us()
            if now >= steady_from_us:
                self.backlog.sample(self.backlog_now())
            if now >= due_us:
                return
            time.sleep(POLL_S)

    def poll(self, what: str, watchdog_s: float, attempt: Callable[[], Any]) -> Any:
        deadline = time.monotonic() + watchdog_s
        while True:
            value = attempt()
            if value is not None:
                return value
            if time.monotonic() >= deadline:
                raise FaultFailed(f'{what} within {watchdog_s:.0f} s')
            time.sleep(POLL_S)

    def recover(self, kind: str, injected_at: int, target_recovered: Callable[[], dict | None],
                workers: list[str] | None = None) -> dict[str, Any]:
        """Wait until service and the target recovered, then hold the stable
        window; answer the recovery evidence."""
        pre_fault_max = self.backlog.pre_fault_max()
        hit = self.in_flight(injected_at)
        if not hit:
            raise FaultFailed(f'{kind} missed active work: nothing was in flight at the injection')
        backlog_recovered_s = None
        target_evidence: dict[str, Any] | None = None

        def attempt() -> dict[str, Any] | None:
            nonlocal backlog_recovered_s, target_evidence
            now = self.clock_us()
            flights = self.in_flight(injected_at)
            unanswered = [flight for flight in flights if flight['ended_at'] is not None and not flight['answered']]
            if unanswered:
                raise FaultFailed(f'{kind} lost answers: {unanswered[:5]}')
            backlog = self.backlog_now()
            if backlog <= pre_fault_max and backlog_recovered_s is None:
                backlog_recovered_s = round((now - injected_at) / 1e6, 3)
            if target_evidence is None:
                target_evidence = target_recovered()
            progress = self.progress(injected_at, workers)
            state = {
                'in_flight_unfinished': sum(1 for flight in flights if flight['ended_at'] is None),
                'turn': progress['turn'] is not None, 'queued': progress['queued'] is not None,
                'cron': progress['cron'] is not None,
                # A rolling deploy also needs a turn answered by the replacement.
                'moved': workers is None or progress['moved'] is not None,
                'backlog': backlog, 'pre_fault_max': pre_fault_max, 'target': target_evidence,
            }
            if not recovered(state):
                return None
            return {**target_evidence, 'progress_s': progress, 'backlog_pre_fault_max': pre_fault_max,
                    'backlog_recovered_s': backlog_recovered_s, 'backlog_at_recovery': backlog,
                    'in_flight_at_injection': len(hit),
                    'hit': [f"{flight['operation']}:{flight['subject']}" for flight in hit][:32],
                    'service_progress_s': progress['turn'],
                    'service_progress_at_us': injected_at + round(progress['turn'] * 1_000_000),
                    'backlog_recovered_at_us': self.clock_us(),
                    'recovered_s': round((now - injected_at) / 1e6, 3)}

        evidence = self.poll(f'{kind} did not recover', self.settle_s, attempt)
        # Stay recovered for the stable window, and record the service rate
        # and latency around the fault beside it.
        stable_from = self.clock_us()
        time.sleep(self.stable_s)
        stable_to = self.clock_us()
        evidence['stable_window'] = self.window(stable_from, stable_to)
        evidence['pre_fault_window'] = self.window(injected_at - self.stable_s * 1_000_000, injected_at)
        return evidence

    def worker_activity(self, generation: str) -> list[dict[str, Any]]:
        activities = []
        for index in range(self.targets['workerCount']):
            url = f'http://{self.name}-worker-{index}-{generation}:18101/load/active'
            activity = self.http('GET', url)
            activities.append({**activity, 'index': index})
        return activities

    def worker_pod(self, index: int, generation: str) -> dict[str, Any]:
        pods = self.cluster.json('pods', '-l', f'app={self.name}-worker-{index},generation={generation}')['items']
        running = [pod for pod in pods if not pod['metadata'].get('deletionTimestamp')]
        if len(running) != 1:
            raise FaultFailed(f'worker {index} of {generation} has {len(running)} pods')
        return running[0]

    def worker_kill(self) -> None:
        kind = 'worker-kill'
        delay = int(self.faults['worker_restart_delay_s'])
        generation = self.targets['generation']
        self.record(kind, 'intent', generation,
                    {'due_s': self.faults['worker_kill_s'], 'restart_delay_s': delay})
        busy = self.poll('no worker ran load work', BUSY_WATCHDOG_S,
                         lambda: choose_worker(self.worker_activity(generation)))
        before = self.worker_pod(busy['index'], generation)
        pod = before['metadata']['name']
        not_before = self.clock_us()
        self.restart_in_place(pod, 'worker', WORKER_PROCESS, 'KILL', kind, delay)
        at = self.record(kind, 'injected', pod, {
            'signal_not_before_us': not_before,
            'pod': pod, 'worker_id': busy['worker_id'], 'active': busy['active'], 'signal': 'KILL',
            'collection_targets': [{'component': 'worker',
                                    'endpoint': f"http://{self.name}-worker-{busy['index']}-control:18101"}],
            'restart_delay_s': delay,
            'restarts_before': container_status(before, 'worker')['restartCount'],
        })

        def target() -> dict[str, Any] | None:
            restart = pod_restart(before, self.cluster.json('pod', pod), 'worker', (137,))
            if restart is None:
                return None
            if not restart['same_pod']:
                raise FaultFailed(f'worker pod {pod} was replaced instead of restarted')
            return {**restart, 'held': self.held(pod, 'worker', kind)}

        evidence = self.recover(kind, at, target)
        if not evidence['held']:
            raise FaultFailed(f'{pod} restarted without its {delay} s hold')
        self.record(kind, 'recovered', pod, evidence)

    def restate_restart(self) -> None:
        kind = 'restate-restart'
        delay = int(self.faults['restate_restart_delay_s'])
        partitions = int(self.targets['partitions'])
        self.record(kind, 'intent', 'restate',
                    {'due_s': self.faults['restate_restart_s'], 'restart_delay_s': delay})
        state_query = ('SELECT partition_id, plain_node_id, gen_node_id, effective_mode, '
                       'leader_epoch, applied_log_lsn FROM partition_state')
        node_query = 'SELECT plain_node_id, gen_node_id, name, state FROM nodes'

        def busy() -> RestateTarget | None:
            first = self.restate(state_query)
            time.sleep(2)
            return choose_restate_target(first, self.restate(state_query), self.restate(node_query))

        chosen = self.poll('no Restate leader was processing work', BUSY_WATCHDOG_S, busy)
        before = self.cluster.json('pod', chosen.pod)
        not_before = self.clock_us()
        self.restart_in_place(chosen.pod, 'restate', RESTATE_PROCESS, 'TERM', kind, delay)
        at = self.record(kind, 'injected', chosen.pod, {
            'signal_not_before_us': not_before,
            'pod': chosen.pod, 'node': chosen.node, 'generation_before': chosen.generation,
            'collection_targets': [
                {'component': 'restate', 'endpoint': f'http://{self.name}-restate:9070'},
                {'component': 'restate', 'endpoint': f'http://{chosen.pod}.{self.name}-peers:5122'},
                {'component': 'restate', 'endpoint': f'http://{chosen.pod}.{self.name}-peers:18102'}],
            'led_partitions': list(chosen.led), 'advancing_partitions': list(chosen.advancing),
            'epochs_before': {str(key): value for key, value in chosen.epochs.items()},
            'signal': 'TERM', 'restart_delay_s': delay,
        })

        def target() -> dict[str, Any] | None:
            restart = pod_restart(before, self.cluster.json('pod', chosen.pod), 'restate', (0, 143))
            if restart is None:
                return None
            if not restart['same_pod']:
                raise FaultFailed(f'Restate pod {chosen.pod} was replaced instead of restarted')
            cluster = restate_recovery(chosen, partitions, self.restate(state_query, avoid=chosen.pod),
                                       self.restate(node_query, avoid=chosen.pod))
            if cluster is None:
                return None
            return {**restart, **cluster, 'held': self.held(chosen.pod, 'restate', kind)}

        evidence = self.recover(kind, at, target)
        if not evidence['held']:
            raise FaultFailed(f'{chosen.pod} restarted without its {delay} s hold')
        self.record(kind, 'recovered', chosen.pod, evidence)

    def helm(self, overlay: dict[str, Any], label: str) -> None:
        path = self.run_dir / f'{label}-values.yaml'
        path.write_text(json.dumps(overlay, indent=2) + '\n')
        values = [item for value in self.args.values for item in ('-f', value)]
        self.cluster.run(['helm', 'upgrade', 'topology', self.args.chart, '--kubeconfig', self.args.kubeconfig,
                          '--namespace', self.args.namespace, *values, '-f', str(path),
                          '--wait', '--timeout', f'{self.settle_s}s'], timeout=self.settle_s + 60)

    def pinned_unfinished(self, deployment: str) -> int:
        answer = self.http('POST', f"{self.targets['restateAdminUrl']}/query", {
            'query': 'SELECT id, status FROM sys_invocation '
                     f"WHERE pinned_deployment_id = '{deployment}' AND status <> 'completed'"})
        return len(answer['rows'])

    def deployment_id(self, uri: str) -> str:
        listing = self.http('GET', f"{self.targets['restateAdminUrl']}/deployments")
        matches = [deployment['id'] for deployment in listing['deployments']
                   if deployment.get('uri', '').rstrip('/') == uri.rstrip('/')]
        if len(matches) != 1:
            raise FaultFailed(f'{len(matches)} Restate deployments are registered at {uri}')
        return matches[0]

    def rolling_deploy(self) -> None:
        kind = 'rolling-deploy'
        old, new = self.targets['generation'], self.targets['rollingGeneration']
        if old == new:
            raise FaultFailed(f'the rolling generation `{new}` already serves')
        pause = int(self.faults['rolling_worker_pause_s'])
        self.record(kind, 'intent', new, {'due_s': self.faults['rolling_deploy_s'],
                                                     'old': old, 'new': new, 'pause_s': pause})
        old_uri = f'http://{self.name}-workers-{old}:18100'
        new_uri = f'http://{self.name}-workers-{new}:18100'
        old_deployment = self.deployment_id(old_uri)
        # Start the replacement beside the running generation: its migrate
        # hook expands the store with the replacement's own operator binary.
        images = {old: self.args.initial_tag, new: self.args.next_tag}
        self.helm({'workers': {'generation': new, 'retainedGenerations': [old], 'generationImages': images}},
                  'rolling')
        old_generation = self.worker_activity(old)[0]['generation']
        new_activity = self.worker_activity(new)
        new_generation = new_activity[0]['generation']
        if old_generation == new_generation:
            raise FaultFailed(f'both builds report drain generation {old_generation}')

        def busy() -> dict[str, Any] | None:
            active = [key for activity in self.worker_activity(old) for key in activity['active']]
            pinned = self.pinned_unfinished(old_deployment)
            return {'active': active, 'pinned': pinned} if active and pinned else None

        work = self.poll('the old generation ran no load work', BUSY_WATCHDOG_S, busy)
        # Registering the replacement's immutable URI moves new admission to
        # it; invocations already pinned to the old deployment stay there.
        registration = self.http('POST', f'http://{self.name}-worker-0-{new}:18101/deployment/register',
                                 {'uri': new_uri})
        # The work the old generation still holds once admission moved.
        work['active'] = [key for activity in self.worker_activity(old) for key in activity['active']]
        if not work['active']:
            raise FaultFailed('the old generation finished its work before admission moved')
        new_workers = [activity['worker_id'] for activity in new_activity]
        at = self.record(kind, 'injected', new_uri, {
            'old_generation': old_generation, 'new_generation': new_generation,
            'collection_targets': [{'component': 'worker',
                                    'endpoint': f"http://{self.name}-worker-{activity['index']}-control:18101"}
                                   for activity in self.worker_activity(old)],
            'old_uri': old_uri, 'new_uri': new_uri, 'old_deployment_id': old_deployment,
            'new_deployment_id': self.deployment_id(new_uri), 'registration': registration,
            'active': work['active'], 'pinned_unfinished_at_move': work['pinned'],
            'new_workers': new_workers,
            'old_workers': [activity['worker_id'] for activity in self.worker_activity(old)],
        })
        time.sleep(pause)
        marked = self.http('POST', f'http://{self.name}-worker-0-{new}:18101/generations/{old_generation}/drain')
        drain_url = f'http://{self.name}-worker-0-{new}:18101/generations/{old_generation}/drain'

        def target() -> dict[str, Any] | None:
            status = self.http('GET', drain_url)
            pinned = self.pinned_unfinished(old_deployment)
            if not status['drained'] or pinned or status['stalled_total']:
                return None
            return {'drained': True, 'drain': status, 'marked': marked, 'pinned_unfinished': pinned,
                    'stalled_total': status['stalled_total']}

        evidence = self.recover(kind, at, target, workers=new_workers)
        # Retire the drained generation: its endpoints stop only now.
        self.helm({'workers': {'generation': new, 'retainedGenerations': [],
                               'generationImages': {new: self.args.next_tag}}}, 'retired')
        remaining = self.cluster.json('deployments', '-l', f'generation={old}')['items']
        if remaining:
            raise FaultFailed(f'{len(remaining)} deployments of {old} survived retirement')
        evidence['retired'] = old
        evidence['pinned_after_retirement'] = self.pinned_unfinished(old_deployment)
        if evidence['pinned_after_retirement']:
            raise FaultFailed(f'{old} gained pinned work after retirement')
        self.record(kind, 'recovered', new_uri, evidence)

    def start(self) -> int:
        """The fault phase starts with the driver's first send."""
        def first() -> int | None:
            rows = self.sql("SELECT min(recorded_at_us) FROM witness_load_events "
                            "WHERE run_id = :'run' AND phase = 'sent'", run=self.args.run)
            return int(rows[0][0]) if rows and rows[0][0] else None
        return self.poll('the driver never sent', self.settle_s, first)

    def run(self) -> None:
        # Faults land in steady load: the fault phase follows the workload's
        # warm-up from the driver's first send.
        steady_from = self.start() + int(self.workload['warmup_s']) * 1_000_000
        self.record('campaign', 'started', self.args.run, {
            'campaign': 'faults', 'workload': self.args.workload_name, 'faults': self.faults,
            'phase_start_us': steady_from,
            'warmup_s': self.workload['warmup_s'],
            'recovery_stable_s': self.stable_s, 'settle_s': self.settle_s,
        })
        schedule = [('worker_kill_s', self.worker_kill), ('restate_restart_s', self.restate_restart),
                    ('rolling_deploy_s', self.rolling_deploy)]
        current = 'campaign'
        try:
            for field, fault in schedule:
                current = field.removesuffix('_s').replace('_', '-')
                self.wait_until(steady_from + int(self.faults[field]) * 1_000_000, steady_from)
                fault()
        except Exception as error:
            kind = {'worker-kill': 'worker-kill', 'restate-restart': 'restate-restart',
                    'rolling-deploy': 'rolling-deploy'}.get(current, 'campaign')
            reason = f'{type(error).__name__}: {error}'
            self.record(kind, 'failed', current, {'reason': reason})
            self.record('campaign', 'failed', self.args.run, {'reason': reason, 'fault': current})
            raise
        self.record('campaign', 'complete', self.args.run, {'faults': len(schedule)})


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__.splitlines()[0])
    parser.add_argument('--kubeconfig', required=True)
    parser.add_argument('--namespace', required=True)
    parser.add_argument('--name', required=True, help='the chart resource name')
    parser.add_argument('--run', required=True, help='the load run ID')
    parser.add_argument('--workload', required=True, help='the workload JSON file')
    parser.add_argument('--workload-name', required=True)
    parser.add_argument('--run-dir', required=True)
    parser.add_argument('--chart', required=True)
    parser.add_argument('--values', action='append', required=True)
    parser.add_argument('--initial-tag', required=True)
    parser.add_argument('--next-tag', required=True)
    args = parser.parse_args()
    try:
        Campaign(args).run()
    except FaultFailed as error:
        print(f'fault campaign failed: {error}', file=sys.stderr)
        return 1
    print('fault campaign complete: worker-kill restate-restart rolling-deploy', flush=True)
    return 0


if __name__ == '__main__':
    sys.exit(main())
