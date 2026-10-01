#!/usr/bin/env python3
"""The rolling-upgrade campaign (FIG-3805 phase B, ADR 0106 §6).

Runs the ADR 0106 choreography against a live load run on the FIG-4167
topology: three Restate nodes, PostgreSQL, and the load driver's sessions
sending through every step. The two builds are the bootstrap image (N) and
the synthetic N+1 image (`synthetic-next`, ADR 0115 §6), each carrying its
own `lash-e2e-worker` and `lashctl`. Worker generations are Kubernetes
names; every one registers its own immutable URI (ADR 0115 §3.5):

  initial   N     the bootstrap generation
  next      N+1   the half roll
  rollback  N     the rollback leg, before finalize
  final     N+1   the roll that finalizes

The steps, in order, each recorded in the witness ledger
(`witness_load_faults`) as intent, injected and recovered:

1. **half-roll:** N+1's `lashctl migrate` expands the store (the chart's
   pre-upgrade hook), N+1 passes `lashctl preflight`, serves beside N,
   registers, and admission moves to it; the forward drain of N's
   generation starts.
2. **rollback:** the forward drain ends, N passes `lashctl preflight` on
   the expanded store and registers again at a URI of its own; N+1's
   generation drains and retires, and its drain mark is ended. Nothing was
   finalized, so N serves everything N+1 wrote.
3. **roll:** N+1's migrate hook and preflight run again, N+1 registers,
   N's generation drains and both N deployments stop taking work. The
   bootstrap N generation retires; the rollback N generation keeps running,
   unregistered, as the stale writer of step 5.
4. **finalize** (FIG-3800 B): refused `deployments_retained` while N's
   deployments are registered; once they are removed, and `drain-status`
   reads N's generation drained, it moves `F` from 1 to 2 with every
   backfill applied (a `generation_not_drained` answer waits for the drain
   again), the forward drain ends, and contract runs.
   The Restate object sweep (FIG-3802, FIG-4041) then lifts every object
   N's format left, and the object preflight lists none.
5. **fence:** the still-running N worker's durable write (a drain mark) is
   refused `WriterFenced` and writes nothing, a fresh N worker process
   refuses the contracted store before it serves, and N's `lashctl
   preflight` refuses it. The stale N generation then retires.

Each step waits for load work on the generation it moves away from, and
recovers only once every operation in flight at it answered, service went on
(a turn, a queued input and a cron emission after it), every session
settled a turn sent after it, and the step's own target holds. The driver
reconciles the same ledger (`load/upgrade_verify.rs`) once the campaign
ends. Every `lashctl` answer is kept in `lashctl.jsonl` in the run
directory.
"""

from __future__ import annotations

import argparse
import json
import shlex
import sys
from pathlib import Path
from typing import Any

from loadtest_faults import BUSY_WATCHDOG_S, Campaign, FaultFailed

CAMPAIGN = 'rolling-upgrade'
STEPS = ('half-roll', 'rollback', 'roll', 'finalize', 'fence')
# The worker generations, and which build each runs.
GENERATIONS = {'initial': 'n', 'next': 'n+1', 'rollback': 'n', 'final': 'n+1'}
# lashctl's pinned exit codes (docs/operations/deploying-and-upgrading.md).
EXIT_DONE, EXIT_REFUSED, EXIT_INCOMPATIBLE, EXIT_NOT_YET = 0, 3, 4, 5
# A fresh worker's store open: long enough to connect and be refused, and
# bounded so a worker that wrongly admits the store fails the step.
FRESH_OPEN_TIMEOUT_S = 60


# -- pure decisions -----------------------------------------------------------


def lashctl_answer(verb: str, code: int, stdout: str) -> dict[str, Any]:
    """A `lashctl --json` envelope, checked to answer `verb`."""
    try:
        body = json.loads(stdout)
    except json.JSONDecodeError as error:
        raise FaultFailed(f'lashctl {verb} printed no JSON envelope (exit {code}): {stdout[-400:]!r}') from error
    if body.get('schema_version') != 1 or body.get('command') != verb:
        raise FaultFailed(f'lashctl {verb} answered a foreign envelope: {body}')
    return body


def refusal(body: dict[str, Any]) -> str | None:
    """The typed refusal a `lashctl` envelope carries, if any."""
    error = body.get('error') or {}
    return (error.get('refusal') or {}).get('refusal')


def finalized(result: dict[str, Any]) -> bool:
    """Finalize moved `F` from 1 to 2 and applied every backfill."""
    backfills = result.get('backfills') or []
    return (result.get('flip') == {'outcome': 'finalized', 'from': 1, 'to': 2}
            and bool(backfills) and all(step.get('state') == 'applied' for step in backfills))


def drained(status: dict[str, Any], pinned_unfinished: int) -> bool:
    """A generation is drained: nothing pinned to it in the store, no
    stalled obligation, and no unfinished invocation on its deployments."""
    return (status.get('drained') is True and not status.get('stalled')
            and not any((status.get('stalled_obligations') or {}).values()) and pinned_unfinished == 0)


def sessions_through(actors: set[str], answered: set[str]) -> list[str]:
    """The sessions (load actors) with no turn answered after a step."""
    return sorted(actors - answered)


def objects_summary(result: dict[str, Any]) -> dict[str, Any]:
    """An `objects-preflight` or `objects-sweep` result as the ledger keeps
    it: counts per family, and at most 20 of what remains. Every object key
    stays in `lashctl.jsonl`; a ledger row travels as one command argument."""
    summary: dict[str, Any] = {}
    if 'families' in result:
        summary['upgraded'] = result.get('upgraded')
        summary['families'] = [{'service': family['service'], 'newest': family['newest'],
                                'objects': family['objects'], 'pending': len(family['pending'])}
                               for family in result['families']]
    if 'swept' in result:
        summary['swept'] = len(result['swept'])
        summary['remaining'] = result['remaining'][:20]
        summary['remaining_total'] = len(result['remaining'])
    return summary


def fenced(text: str) -> bool:
    """A write refused by the fleet-format writer fence (ADR 0115 L2):
    `StoreError::WriterFenced`'s message."""
    return 'writer fenced: the fleet epoch is' in text


def refused_store(text: str) -> bool:
    """A store open refused by the compatibility contract: the contracted
    reader floor (`CompatRefusal::ReaderFloorAbove`), or the finalized fleet
    epoch outside N's writable range (`CompatRefusal::FleetOutsideWritable`)."""
    return any(marker in text for marker in ('with reader floor', 'store records fleet epoch'))


# -- the campaign -------------------------------------------------------------


class UpgradeCampaign(Campaign):
    def __init__(self, args: argparse.Namespace):
        super().__init__(args)
        self.lashctl_log = self.run_dir / 'lashctl.jsonl'
        self.tags = {'n': args.initial_tag, 'n+1': args.next_tag}
        self.bootstrap = self.targets['generation']
        if self.bootstrap != 'initial':
            raise FaultFailed(f'the rolling-upgrade campaign starts from generation `initial`, not `{self.bootstrap}`')
        self.admin = self.targets['restateAdminUrl']
        self.ingress = self.admin.replace(':9070', ':8080')
        # Each build's drain generation `G`, read from its workers.
        self.g: dict[str, str] = {}
        self.steady_from = 0

    # -- in-cluster hands --

    def uri(self, generation: str) -> str:
        return f'http://{self.name}-workers-{generation}:18100'

    def control(self, generation: str, path: str) -> str:
        return f'http://{self.name}-worker-0-{generation}:18101{path}'

    def deploy(self, current: str, retained: list[str], label: str, migrate: bool) -> None:
        """Serve `current` beside `retained` through the chart; `migrate`
        runs `current`'s own `lashctl migrate` first as the pre-upgrade
        hook."""
        images = {generation: self.tags[GENERATIONS[generation]] for generation in [current, *retained]}
        self.helm({'workers': {'generation': current, 'retainedGenerations': retained,
                               'generationImages': images, 'migrate': migrate}}, label)
        for generation in GENERATIONS:
            if generation != current and generation not in retained:
                remaining = self.cluster.json('deployments', '-l', f'generation={generation}')['items']
                if remaining:
                    raise FaultFailed(f'{len(remaining)} deployments of {generation} survived {label}')

    def migrate_hook(self, generation: str) -> dict[str, Any]:
        """The envelope the pre-upgrade hook's `lashctl migrate` printed."""
        logs = self.cluster.kubectl('logs', f'job/{self.name}-migrate-{generation}').stdout
        body = lashctl_answer('migrate', 0, logs.strip().splitlines()[-1] if logs.strip() else '')
        if body.get('error') is not None:
            raise FaultFailed(f'the {generation} migrate hook failed: {body}')
        self.log_lashctl(generation, ['migrate'], 0, body)
        return body['result']

    def log_lashctl(self, generation: str, args: list[str], code: int, body: dict[str, Any]) -> None:
        row = {'generation': generation, 'build': GENERATIONS[generation], 'args': args, 'exit': code,
               'body': body}
        with self.lashctl_log.open('a') as log:
            log.write(json.dumps(row, sort_keys=True) + '\n')
        print(f'lashctl[{generation}] {shlex.join(args)} --json: exit={code} {json.dumps(body, sort_keys=True)}',
              flush=True)

    def lashctl(self, generation: str, *args: str, expect: tuple[int, ...] = (EXIT_DONE,)
                ) -> tuple[int, dict[str, Any]]:
        """Run `generation`'s own `lashctl` inside its first worker's pod,
        over the run's PostgreSQL store, and require an expected exit."""
        result = self.cluster.kubectl(
            'exec', f'deployment/{self.name}-worker-0-{generation}', '-c', 'worker', '--', 'bash', '-c',
            'LASH_POSTGRES_DATABASE_URL="$DATABASE_URL" exec lashctl "$@" --json', 'lashctl', *args,
            check=False, timeout=self.settle_s)
        body = lashctl_answer(args[0], result.returncode, result.stdout)
        self.log_lashctl(generation, list(args), result.returncode, body)
        if result.returncode not in expect:
            raise FaultFailed(f'lashctl {shlex.join(args)} on {generation} exited {result.returncode}, '
                              f'expected {expect}: {body}')
        return result.returncode, body

    def answer(self, method: str, url: str) -> tuple[int, str]:
        """An HTTP status and body, whatever the status."""
        result = self.probe('curl -sS -X "$1" -o /tmp/answer -w "%{http_code}" "$2"; printf "\\n"; cat /tmp/answer',
                            method, url)
        status, _, body = result.stdout.partition('\n')
        return int(status), body

    def generation_of(self, generation: str) -> str:
        found = {activity['generation'] for activity in self.worker_activity(generation)}
        if len(found) != 1:
            raise FaultFailed(f'the {generation} workers report drain generations {sorted(found)}')
        return found.pop()

    def remove_deployment(self, uri: str) -> str:
        deployment = self.deployment_id(uri)
        self.probe('curl -fsS -X DELETE "$1"', f'{self.admin}/deployments/{deployment}?force=true')
        return deployment

    def actors(self) -> set[str]:
        """Every session (load actor) that has sent a turn."""
        rows = self.sql("SELECT DISTINCT split_part(subject, '/', 2) FROM witness_load_events "
                        "WHERE run_id = :'run' AND operation = 'turn' AND phase = 'sent'", run=self.args.run)
        return {row[0] for row in rows}

    def answered_actors(self, at: int) -> set[str]:
        """The sessions with a turn sent after `at` that settled: answered,
        or cancelled as its plan asked (the driver's verifier holds each
        turn to its plan)."""
        rows = self.sql(self.OPERATIONS + """
            SELECT DISTINCT split_part(sent.subject, '/', 2)
            FROM sent JOIN ended USING (operation, subject)
            WHERE sent.operation = 'turn' AND sent.at > :at
              AND ended.response -> 'outcome' ->> 'status' IN ('answered', 'cancelled')
        """, run=self.args.run, at=at)
        return {row[0] for row in rows}

    def sessions_evidence(self, at: int, actors: set[str]) -> dict[str, Any] | None:
        missing = sessions_through(actors, self.answered_actors(at))
        return None if missing else {'sessions': sorted(actors)}

    def busy_on(self, generation: str, deployment: str) -> dict[str, Any]:
        """Wait for load work running on `generation` and pinned to its
        deployment: the work the step moves away from."""
        def busy() -> dict[str, Any] | None:
            active = [key for activity in self.worker_activity(generation) for key in activity['active']]
            pinned = self.pinned_unfinished(deployment)
            return {'active': active, 'pinned': pinned} if active and pinned else None
        return self.poll(f'{generation} ran no load work', BUSY_WATCHDOG_S, busy)

    def in_flight_now(self) -> None:
        """Wait until the witness shows load work in flight."""
        self.poll('no load operation was in flight', BUSY_WATCHDOG_S,
                  lambda: True if self.in_flight(self.clock_us()) else None)

    def settle(self) -> None:
        """Sample the healthy backlog between steps."""
        self.wait_until(self.clock_us(), self.steady_from)

    # -- the steps --

    def half_roll(self) -> None:
        step, old, new = 'half-roll', 'initial', 'next'
        self.record(step, step, 'intent', new, {'old': old, 'new': new})
        old_deployment = self.deployment_id(self.uri(old))
        self.deploy(new, [old], step, migrate=True)
        migrate = self.migrate_hook(new)
        _, preflight = self.lashctl(new, 'preflight')
        _, version = self.lashctl(new, 'version')
        self.g['n'], self.g['n+1'] = self.generation_of(old), self.generation_of(new)
        if self.g['n'] == self.g['n+1']:
            raise FaultFailed(f"both builds report drain generation {self.g['n']}")
        work = self.busy_on(old, old_deployment)
        registration = self.http('POST', self.control(new, '/deployment/register'), {'uri': self.uri(new)})
        _, drain = self.lashctl(new, 'drain', self.g['n'])
        new_workers = [activity['worker_id'] for activity in self.worker_activity(new)]
        actors = self.actors()
        at = self.record(step, step, 'injected', self.uri(new), {
            'old_generation': self.g['n'], 'new_generation': self.g['n+1'], 'old_uri': self.uri(old),
            'new_uri': self.uri(new), 'old_deployment_id': old_deployment,
            'new_deployment_id': self.deployment_id(self.uri(new)), 'registration': registration,
            'active': work['active'], 'pinned_unfinished_at_move': work['pinned'], 'new_workers': new_workers,
            'migrate': migrate, 'preflight': preflight['result'], 'version': version['result'],
            'drain': drain['result'], 'sessions': sorted(actors),
        })
        evidence = self.recover(step, at, lambda: self.sessions_evidence(at, actors), workers=new_workers)
        self.record(step, step, 'recovered', self.uri(new), {**evidence, 'migrated_by': 'n+1',
                                                              'forward_drain': self.g['n']})

    def rollback(self) -> None:
        step, old, new = 'rollback', 'next', 'rollback'
        self.record(step, step, 'intent', new, {'old': old, 'new': new})
        # The forward drain ends: N comes back before anything finalized.
        _, ended = self.lashctl(old, 'end-drain', self.g['n'])
        self.deploy(new, ['initial', old], step, migrate=False)
        _, preflight = self.lashctl(new, 'preflight')
        restored = self.generation_of(new)
        if restored != self.g['n']:
            raise FaultFailed(f"the rollback serves generation {restored}, not N's {self.g['n']}")
        old_deployment = self.deployment_id(self.uri(old))
        work = self.busy_on(old, old_deployment)
        registration = self.http('POST', self.control(new, '/deployment/register'), {'uri': self.uri(new)})
        # N's operator drains N+1's generation: the fleet is going back.
        _, drain = self.lashctl(new, 'drain', self.g['n+1'])
        new_workers = [activity['worker_id'] for activity in self.worker_activity(new)]
        actors = self.actors()
        at = self.record(step, step, 'injected', self.uri(new), {
            'old_generation': self.g['n+1'], 'new_generation': restored, 'restored_generation': restored,
            'n_generation': self.g['n'], 'old_uri': self.uri(old), 'new_uri': self.uri(new),
            'old_deployment_id': old_deployment, 'new_deployment_id': self.deployment_id(self.uri(new)),
            'registration': registration, 'active': work['active'],
            'pinned_unfinished_at_move': work['pinned'], 'new_workers': new_workers,
            'forward_drain_ended': ended['result'], 'preflight': preflight['result'], 'drain': drain['result'],
            'sessions': sorted(actors),
        })
        drain_status: dict[str, Any] = {}

        def target() -> dict[str, Any] | None:
            nonlocal drain_status
            _, status = self.lashctl(new, 'drain-status', self.g['n+1'], '--restate-admin-url', self.admin, expect=(EXIT_DONE, EXIT_NOT_YET))
            drain_status = status['result']
            pinned = self.pinned_unfinished(old_deployment)
            if not drained(drain_status, pinned):
                return None
            sessions = self.sessions_evidence(at, actors)
            if sessions is None:
                return None
            return {**sessions, 'drained': True, 'drain_status': drain_status, 'pinned_unfinished': pinned,
                    'stalled_total': sum((drain_status.get('stalled_obligations') or {}).values())}

        evidence = self.recover(step, at, target, workers=new_workers)
        # N+1 retires: its endpoints stop, its deployment leaves Restate, and
        # its drain mark ends. Nothing N+1 wrote is lost to N.
        self.deploy(new, ['initial'], 'rollback-retired', migrate=False)
        evidence['retired_deployment_id'] = self.remove_deployment(self.uri(old))
        evidence['pinned_after_retirement'] = self.pinned_unfinished(old_deployment)
        if evidence['pinned_after_retirement']:
            raise FaultFailed(f'{old} gained pinned work after retirement')
        _, reverse_ended = self.lashctl(new, 'end-drain', self.g['n+1'])
        evidence['reverse_drain_ended'] = reverse_ended['result']
        evidence['retired'] = old
        self.record(step, step, 'recovered', self.uri(new), evidence)

    def roll(self) -> None:
        step, new = 'roll', 'final'
        self.record(step, step, 'intent', new, {'old': ['initial', 'rollback'], 'new': new})
        deployments = {generation: self.deployment_id(self.uri(generation)) for generation in ('initial', 'rollback')}
        self.deploy(new, ['initial', 'rollback'], step, migrate=True)
        migrate = self.migrate_hook(new)
        _, preflight = self.lashctl(new, 'preflight')
        if self.generation_of(new) != self.g['n+1']:
            raise FaultFailed(f"the roll serves {self.generation_of(new)}, not N+1's {self.g['n+1']}")
        work = self.busy_on('rollback', deployments['rollback'])
        registration = self.http('POST', self.control(new, '/deployment/register'), {'uri': self.uri(new)})
        _, drain = self.lashctl(new, 'drain', self.g['n'])
        new_workers = [activity['worker_id'] for activity in self.worker_activity(new)]
        actors = self.actors()
        at = self.record(step, step, 'injected', self.uri(new), {
            'old_generation': self.g['n'], 'new_generation': self.g['n+1'], 'old_uri': self.uri('rollback'),
            'new_uri': self.uri(new), 'old_deployment_ids': deployments,
            'new_deployment_id': self.deployment_id(self.uri(new)), 'registration': registration,
            'active': work['active'], 'pinned_unfinished_at_move': work['pinned'], 'new_workers': new_workers,
            'migrate': migrate, 'preflight': preflight['result'], 'drain': drain['result'],
            'sessions': sorted(actors),
        })

        def target() -> dict[str, Any] | None:
            _, status = self.lashctl(new, 'drain-status', self.g['n'], '--restate-admin-url', self.admin, expect=(EXIT_DONE, EXIT_NOT_YET))
            pinned = sum(self.pinned_unfinished(deployment) for deployment in deployments.values())
            if not drained(status['result'], pinned):
                return None
            sessions = self.sessions_evidence(at, actors)
            if sessions is None:
                return None
            return {**sessions, 'drained': True, 'drain_status': status['result'], 'pinned_unfinished': pinned,
                    'stalled_total': sum((status['result'].get('stalled_obligations') or {}).values())}

        evidence = self.recover(step, at, target, workers=new_workers)
        # The bootstrap N generation retires; the rollback N generation stays
        # up, and is unregistered at finalize, to be step 5's stale writer.
        self.deploy(new, ['rollback'], 'roll-retired', migrate=False)
        evidence['retired'] = 'initial'
        self.record(step, step, 'recovered', self.uri(new), evidence)

    def finalize(self) -> None:
        step, operator = 'finalize', 'final'
        self.record(step, step, 'intent', operator, {'retired_generation': self.g['n']})
        finalize = ['finalize', self.g['n'], '--restate-admin-url', self.admin]
        # Retirement is read from the engine: N's stopped deployments are
        # still registered, so finalize refuses and moves nothing.
        _, retained = self.finalize_when_drained(operator, finalize, EXIT_REFUSED)
        if refusal(retained) != 'deployments_retained':
            raise FaultFailed(f'finalize ran with N registered: {retained}')
        removed = {generation: self.remove_deployment(self.uri(generation)) for generation in ('initial', 'rollback')}
        actors = self.actors()
        self.in_flight_now()
        attempts, flip = self.finalize_when_drained(operator, finalize, EXIT_DONE)
        at = self.record(step, step, 'injected', operator, {
            'retired_generation': self.g['n'], 'refused': refusal(retained), 'removed_deployment_ids': removed,
            'finalize': flip['result'], 'not_yet_attempts': attempts,
            'sessions': sorted(actors), 'active': [],
        })
        if not finalized(flip['result']):
            raise FaultFailed(f'finalize did not move F from 1 to 2 with every backfill applied: {flip}')
        _, ended = self.lashctl(operator, 'end-drain', self.g['n'])
        _, contract = self.lashctl(operator, 'migrate', '--phase', 'contract')
        if not contract['result'].get('executed'):
            raise FaultFailed(f'contract ran nothing after its backfills: {contract}')
        # The Restate objects N's format left upgrade through the sweep.
        restate = ['--restate-admin-url', self.admin]
        _, before = self.lashctl(operator, 'objects-preflight', *restate, expect=(EXIT_DONE, EXIT_NOT_YET))
        _, sweep = self.lashctl(operator, 'objects-sweep', *restate, '--restate-ingress-url', self.ingress)
        _, after = self.lashctl(operator, 'objects-preflight', *restate)
        evidence = self.recover(step, at, lambda: self.sessions_evidence(at, actors))
        self.record(step, step, 'recovered', operator, {
            **evidence, 'finalized': True, 'flip': flip['result']['flip'],
            'backfills': flip['result']['backfills'], 'forward_drain_ended': ended['result'],
            'contract': contract['result'], 'objects_before': objects_summary(before['result']),
            'objects_sweep': objects_summary(sweep['result']), 'objects_after': objects_summary(after['result']),
            'objects_upgraded': after['result'].get('upgraded') is True,
        })

    def finalize_when_drained(self, operator: str, finalize: list[str], expect: int
                              ) -> tuple[list[dict[str, Any]], dict[str, Any]]:
        """Run `lashctl finalize` as it directs: once `drain-status` reads N's
        generation drained, requiring the exit `expect`. N's deployments stay
        registered until finalize and serve N's generation lanes, so N's work
        can reach the store again for a moment (a session close), which
        finalize answers `generation_not_drained`, exit 5; that waits for the
        drain again, within the step's watchdog. Answers every such answer,
        and the envelope finalize ended with."""
        attempts: list[dict[str, Any]] = []

        def attempt() -> dict[str, Any] | None:
            _, status = self.lashctl(operator, 'drain-status', self.g['n'], '--restate-admin-url', self.admin, expect=(EXIT_DONE, EXIT_NOT_YET))
            if not drained(status['result'], 0):
                return None
            code, body = self.lashctl(operator, *finalize, expect=(expect, EXIT_NOT_YET))
            if code == EXIT_NOT_YET and expect != EXIT_NOT_YET:
                if refusal(body) != 'generation_not_drained':
                    raise FaultFailed(f'finalize answered not_yet for another reason: {body}')
                attempts.append(body['error'])
                return None
            return body

        body = self.poll(f"finalize found {self.g['n']} drained", self.settle_s, attempt)
        return attempts, body

    def fence(self) -> None:
        step, stale = 'fence', 'rollback'
        self.record(step, step, 'intent', stale, {'stale_generation': self.g['n']})
        actors = self.actors()
        self.in_flight_now()
        # The live N worker is still up and still holds its store: its next
        # durable write, a drain mark, is fenced and writes nothing.
        status, body = self.answer('POST', self.control(stale, f"/generations/{self.g['n+1']}/drain"))
        at = self.record(step, step, 'injected', stale, {
            'stale_generation': self.g['n'], 'live_write': {'status': status, 'body': body[-2000:]},
            'sessions': sorted(actors), 'active': [],
        })
        if status < 500 or not fenced(body):
            raise FaultFailed(f'the live N worker wrote after finalize: HTTP {status} {body}')
        _, mark = self.lashctl('final', 'drain-status', self.g['n+1'], '--restate-admin-url', self.admin, expect=(EXIT_DONE, EXIT_NOT_YET))
        if mark['result'].get('draining_since_ms') is not None:
            raise FaultFailed(f'the fenced drain mark was written: {mark}')
        # A fresh N process refuses the store before it serves anything.
        fresh = self.cluster.kubectl(
            'exec', f'deployment/{self.name}-worker-0-{stale}', '-c', 'worker', '--', 'bash', '-c',
            f'WORKER_PORT=18200 WORKER_CONTROL_PORT=18201 timeout {FRESH_OPEN_TIMEOUT_S} lash-e2e-worker 2>&1',
            check=False, timeout=FRESH_OPEN_TIMEOUT_S + 30)
        if fresh.returncode == 0 or fresh.returncode == 124 or not refused_store(fresh.stdout):
            raise FaultFailed(f'a fresh N worker opened the finalized store (exit {fresh.returncode}): '
                              f'{fresh.stdout[-2000:]}')
        code, preflight = self.lashctl(stale, 'preflight', expect=(EXIT_REFUSED, EXIT_INCOMPATIBLE))
        evidence = self.recover(step, at, lambda: self.sessions_evidence(at, actors))
        self.deploy('final', [], 'fence-retired', migrate=False)
        self.record(step, step, 'recovered', stale, {
            **evidence, 'live_writer_fenced': True, 'drain_mark_written': False,
            'fresh_open': {'exit': fresh.returncode, 'output': fresh.stdout[-2000:]},
            'fresh_open_refused': True, 'operator_preflight': {'exit': code, 'body': preflight},
            'retired': stale,
        })

    def run(self) -> None:
        self.steady_from = self.start() + int(self.workload['warmup_s']) * 1_000_000
        self.record('campaign', 'campaign', 'started', self.args.run, {
            'campaign': CAMPAIGN, 'workload': self.args.workload_name, 'steps': list(STEPS),
            'generations': GENERATIONS, 'phase_start_us': self.steady_from,
            'warmup_s': self.workload['warmup_s'], 'recovery_stable_s': self.stable_s, 'settle_s': self.settle_s,
        })
        current = 'campaign'
        try:
            self.wait_until(self.steady_from, self.steady_from)
            for current, step in zip(STEPS, (self.half_roll, self.rollback, self.roll, self.finalize, self.fence)):
                self.settle()
                step()
        except Exception as error:
            reason = f'{type(error).__name__}: {error}'
            self.record(current, current, 'failed', current, {'reason': reason})
            self.record('campaign', 'campaign', 'failed', self.args.run, {'reason': reason, 'step': current})
            raise
        self.record('campaign', 'campaign', 'complete', self.args.run, {'steps': len(STEPS)})


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
    parser.add_argument('--initial-tag', required=True, help="N's image tag")
    parser.add_argument('--next-tag', required=True, help="the synthetic N+1's image tag")
    args = parser.parse_args()
    try:
        UpgradeCampaign(args).run()
    except FaultFailed as error:
        print(f'rolling-upgrade campaign failed: {error}', file=sys.stderr)
        return 1
    print(f"rolling-upgrade campaign complete: {' '.join(STEPS)}", flush=True)
    return 0


if __name__ == '__main__':
    sys.exit(main())
