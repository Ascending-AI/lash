#!/usr/bin/env python3
"""Reconcile FIG-3790 smoke measurements. This command establishes no budgets."""
import argparse
import base64
from collections import Counter, defaultdict
import hashlib
import json
import math
from pathlib import Path
import re

TERMINAL = {'answered', 'failed', 'cancelled', 'completed'}
from loadtest_ledger import WITNESS_CLASSES, FAULT_CLASSES, require_pair
OUTCOMES = TERMINAL | {'parked', 'stalled', 'unrecognized', 'timeout', 'client_error'}
LEDGERS = {'witness_load_events', 'witness_provider_receipts', 'witness_effect_attempts',
           'witness_effect_commits', 'witness_effect_replies', 'witness_load_faults'}
RECORDS = {'run', 'operation', 'sample', 'witness', 'witness_evidence', 'sample_error', 'query_retry', 'clock_anchor'}


def require(condition, message):
    if not condition:
        raise ValueError(message)


def version(row):
    require(row.get('schema_version') == 1, 'unsupported measurement schema')


def distribution(values):
    values = sorted(values)
    return {'count': len(values), **{key: values[max(0, math.ceil(len(values) * fraction) - 1)] if values else None
                                  for key, fraction in [('p50', .5), ('p95', .95), ('p99', .99)]}}


def populations(rows):
    seen = set()
    outcomes = Counter()
    accepted = 0
    for row in rows:
        version(row)
        require(row['id'] not in seen, f"duplicate operation {row['id']}")
        seen.add(row['id'])
        try:
            times = [row['scheduled_ns'], row['sent_ns']]
            if row['accepted_ns'] is not None:
                accepted += 1
                times.append(row['accepted_ns'])
            times.append(row['observed_ns'])
        except KeyError as error:
            raise ValueError(f'missing operation timestamp {error}') from error
        require(all(isinstance(value, int) and value >= 0 for value in times), 'invalid monotonic timestamp')
        require(times == sorted(times), f"clock order violated for {row['id']}")
        require(row['outcome'] in OUTCOMES, 'unrecognized measurement outcome')
        outcomes[row['outcome']] += 1
        require(row['outcome'] not in TERMINAL or row['accepted_ns'] is not None, 'terminal without acceptance')
    return {'offered': len(rows), 'accepted': accepted,
            'durable_terminal': sum(outcomes[key] for key in TERMINAL),
            'outcomes': dict(outcomes), 'unresolved': len(rows) - sum(outcomes[key] for key in TERMINAL)}


class CounterDeltas:
    """Every identity has an explicit epoch; a new epoch marks an unobserved gap."""
    def __init__(self):
        self.values = {}
        self.epochs = {}
        self.total = 0
        self.gaps = 0

    def add(self, identity, epoch, value):
        require(math.isfinite(value) and value >= 0, 'invalid counter value')
        previous_epoch = self.epochs.get(identity)
        if previous_epoch is not None and previous_epoch != epoch:
            self.gaps += 1
        self.epochs[identity] = epoch
        key = (identity, epoch)
        previous = self.values.get(key, value)
        require(value >= previous, f'counter reset without epoch: {identity}')
        delta = value - previous
        self.values[key] = value
        self.total += delta
        return delta


def validate_resources(resources):
    processes = resources['processes']
    require(len({row['pid'] for row in processes}) == len(processes), 'duplicate child PID')
    for name, total in [('rss_bytes', 'rss_sum_bytes'), ('cpu_ticks', 'cpu_ticks_sum')]:
        require(sum(row[name] for row in processes) == resources[total], f'parent/child {name} total mismatch')


POOL_COUNTER_UNITS = {name: 'count' for name in (
    'checkouts', 'queue_waits', 'ipc_sent_messages', 'ipc_received_messages', 'resets', 'reuses',
    'crashes', 'discards', 'replacements', 'cell_executions', 'process_executions', 'receipts_dropped')}
POOL_COUNTER_UNITS.update(queue_delay_ns='nanoseconds', ipc_sent_bytes='bytes', ipc_received_bytes='bytes')
POOL_GAUGE_UNITS = dict(workers='count', idle='count', queued_items='count', queued_bytes='bytes')


def pool_identity(worker):
    resources = worker['resources']
    pids = {row['pid'] for row in resources['processes']}
    parents = [row for row in resources['processes'] if row['parent_pid'] not in pids]
    require(len(parents) == 1, 'pool has no unique parent process')
    generation = resources.get('generation')
    require(isinstance(generation, str) and generation, 'pool has no worker generation')
    return (worker['node'], generation, parents[0]['epoch'], resources['pool']['epoch'])


POOL_EPOCH_CHANGED = 'pool epoch changed with an unobserved counter interval'
MISSING_INVOKER_EXPORTER = 'missing Restate invoker exporter'


def pool_report(run, operations, samples):
    """Require exporter-backed counters and independent workload/child evidence.

    A worker's pool identity (generation, parent start epoch, pool epoch) may
    change between two samples. Each change is returned in `epoch_gaps` as a
    counter gap with an unknown delta; `summarize` decides whether a fault on
    that worker explains it."""
    counters = defaultdict(CounterDeltas)
    receipts = {}
    child_epochs = set()
    epochs = {}
    gaps = []
    series = []
    classes = Counter()
    unsampled_classes = Counter()
    try:
        for sample in samples:
            for worker in sample['workers']:
                resources = worker['resources']
                pool = resources.get('pool')
                require(isinstance(pool, dict) and pool.get('exporter') == 'lash-vm-client/pool-v1',
                        'missing pool exporter evidence')
                require(isinstance(pool.get('epoch'), int) and pool['epoch'] > 0, 'missing pool epoch')
                identity = pool_identity(worker)
                expected_units = {**POOL_GAUGE_UNITS, **POOL_COUNTER_UNITS}
                require(pool.get('units') == expected_units, 'missing or invalid pool metric units')
                values = pool.get('counters', {})
                for name in expected_units:
                    value = pool.get(name) if name in POOL_GAUGE_UNITS else values.get(name)
                    require(type(value) is int and value >= 0, f'missing or invalid pool metric {name}')
                require(pool['idle'] <= pool['workers'], 'pool idle workers exceed workers')
                require(pool['queued_items'] != 0 or pool['queued_bytes'] == 0, 'pool queue occupancy mismatch')
                require(values['receipts_dropped'] == 0, 'pool execution receipt overflow')
                previous = epochs.get(worker['node'])
                if previous is not None and previous['identity'] != identity:
                    describe = lambda row: {'epoch': dict(zip(('generation', 'parent_epoch', 'pool_epoch'), row['identity'][1:])),
                                            'value': row['counters']}
                    gaps.append(dict(schema_version=1, record='counter_gap', run=run['run'], sample_kind='periodic',
                                     monotonic_ns=worker['observed_ns'], previous_observed_ns=previous['observed_ns'],
                                     target={'component': 'worker', 'endpoint': worker['node']},
                                     counter='pool_identity', identity=worker['node'], complete=False,
                                     before=describe(previous),
                                     after=describe(dict(identity=identity, counters=values)), unobserved_delta=None))
                epochs[worker['node']] = dict(identity=identity, counters=values, observed_ns=worker['observed_ns'])
                for name in POOL_COUNTER_UNITS:
                    counters[name].add(identity, identity, values[name])
                execution_rows = pool.get('executions')
                require(isinstance(execution_rows, list), 'missing pool execution receipts')
                observed_classes = Counter(row.get('class_name') for row in execution_rows)
                require(set(observed_classes) <= {'cell', 'process'}, 'unknown pool execution class')
                for kind in ('cell', 'process'):
                    require(observed_classes[kind] == values[kind + '_executions'], 'pool execution counter/receipt mismatch')
                leases = set()
                for row in execution_rows:
                    lease = row.get('lease')
                    require(type(lease) is int and lease > 0 and lease not in leases, 'invalid or duplicate pool execution lease')
                    leases.add(lease)
                    key = (*identity, lease)
                    require(receipts.get(key, row) == row, 'pool execution receipt changed')
                    receipts[key] = row
                pids = {row['pid'] for row in resources['processes']}
                for proc in resources['processes']:
                    if proc['parent_pid'] in pids:
                        child_epochs.add((*identity, proc['pid'], proc['epoch']))
                series.append(dict(node=identity[0], generation=identity[1], parent_epoch=identity[2],
                                   pool_epoch=identity[3], observed_ns=worker['observed_ns'],
                                   gauges={name: pool[name] for name in POOL_GAUGE_UNITS}, counters=values))
        sessions, processes = set(), set()
        def identities(value):
            if isinstance(value, dict):
                for name, item in value.items():
                    if name == 'session_id' and isinstance(item, str):
                        sessions.add(item)
                    if name == 'process_id' and isinstance(item, str):
                        processes.add(item)
                    if name in {'started_process_ids', 'started'} and isinstance(item, list):
                        processes.update(text for text in item if isinstance(text, str))
                    identities(item)
            elif isinstance(value, list):
                for item in value:
                    identities(item)
        for row in operations:
            identities(row['request'])
            identities(row['response'])
        matched = []
        for key, row in receipts.items():
            owner = row.get('owner')
            require(isinstance(owner, str), 'pool receipt has no owner')
            owned = (row['class_name'] == 'cell' and any(owner.startswith('rlm:' + session + ':') for session in sessions)
                     or row['class_name'] == 'process' and owner.removeprefix('process:') in processes and owner.startswith('process:'))
            if not owned:
                continue
            sampled = (*key[:-1], row.get('pid'), row.get('process_epoch')) in child_epochs
            (classes if sampled else unsampled_classes)[row['class_name']] += 1
            matched.append(dict(node=key[0], generation=key[1], parent_epoch=key[2], pool_epoch=key[3],
                                resource_sampled=sampled, **row))
        require(classes['cell'] > 0 and classes['process'] > 0,
                'missing independently sampled run-owned pool cell or process execution evidence')
        require(all(counters[name].total > 0 for name in ('ipc_sent_messages', 'ipc_sent_bytes', 'ipc_received_messages', 'ipc_received_bytes')),
                'pool execution has no IPC traffic in the collection interval')
        return dict(status='PASSED', reasons=[], units={**POOL_GAUGE_UNITS, **POOL_COUNTER_UNITS},
                    counters={name: counter.total for name, counter in sorted(counters.items())},
                    execution_classes=dict(classes), unsampled_execution_classes=dict(unsampled_classes),
                    executions=matched, samples=series, epoch_gaps=gaps)
    except (ValueError, KeyError, TypeError) as error:
        return dict(status='INCOMPLETE', reasons=[str(error)], execution_classes=dict(classes), executions=[], samples=series,
                    epoch_gaps=gaps)


def deployment_memory(resources):
    return sum(row['cgroup_memory_bytes'] for row in resources)


def journal_summary(rows, completed):
    maxima = {}
    growth = []
    for row in rows:
        value = row['journal']
        previous = maxima.setdefault(row['id'], {'entries': 0, 'bytes': 0})
        require(value['entries'] >= 0 and value['bytes'] >= 0, 'negative journal size')
        growth.append({'id': row['id'], 'bytes': max(0, value['bytes'] - previous['bytes'])})
        for name in ('entries', 'bytes'):
            previous[name] = max(previous[name], value[name])
    total = sum(row['bytes'] for row in maxima.values())
    return {'invocations': len(maxima), 'maxima': maxima, 'growth': growth,
            'logical_bytes': total, 'bytes_per_terminal': total / completed if completed else None}


def prometheus(text):
    rows = []
    for line in text.splitlines():
        if not line or line.startswith('#'):
            continue
        match = re.fullmatch(r'([a-zA-Z_:][\w:]*)(\{.*\})?\s+(\S+)(?:\s+\S+)?', line)
        require(match is not None, f'invalid Prometheus sample: {line[:100]}')
        name, labels, value = match.groups()
        labels = {key: json.loads('"' + val + '"') for key, val in
                  re.findall(r'(\w+)="((?:[^"\\]|\\.)*)"', labels or '')}
        value = float(value)
        # Empty quantiles may be NaN. Counters required below must be finite.
        if name.endswith(('_total', '_bytes')) or not labels:
            require(math.isfinite(value), f'nonfinite metric {name}')
        rows.append((name, labels, value))
    return rows


def normalize_fault_rows(rows, anchors, run):
    """One recorded offset per row. The read interval and witness precision bound it."""
    index = {}
    errors = []
    for anchor in anchors:
        version(anchor)
        require(anchor['run'] == run and anchor['clock'] == 'witness_postgres_us', 'mismatched anchor clock or run')
        require(anchor['id'] not in index, 'duplicate clock anchor')
        require(all(type(anchor[key]) is int and anchor[key] >= 0
                    for key in ['witness_us', 'monotonic_ns', 'round_trip_ns']), 'invalid clock anchor')
        index[anchor['id']] = anchor
    normalized = []
    seen = set()
    for row in rows:
        require_pair('faults', row['kind'], row['phase'])
        require(row['fault_event_id'] not in seen, 'duplicate fault event')
        seen.add(row['fault_event_id'])
        identity = f"{row['kind']}/{row['phase']}/{row['fault_event_id']}"
        require(row['run_id'] == run, 'mixed fault run identities')
        anchor = index.get(str(row['fault_event_id']))
        if anchor is None:
            errors.append(identity + ': missing clock anchor')
            continue
        uncertainty = (anchor['round_trip_ns'] + 1) // 2 + 1000
        start = index.get('campaign-start')
        if start is not None:
            start_bound = (start['round_trip_ns'] + 1) // 2 + 1000
            offset = anchor['monotonic_ns'] - anchor['witness_us'] * 1000
            start_offset = start['monotonic_ns'] - start['witness_us'] * 1000
            if abs(offset - start_offset) > uncertainty + start_bound:
                errors.append(identity + ': witness clock offset moved beyond anchor bounds')
                continue
        def convert(witness_us):
            require(type(witness_us) is int, 'invalid witness timestamp')
            instant = anchor['monotonic_ns'] + (witness_us - anchor['witness_us']) * 1000
            return instant, [instant - uncertainty, instant + uncertainty]
        instant, bounds = convert(row['recorded_at_us'])
        if bounds[0] < 0:
            errors.append(identity + ': normalization precedes driver origin within its bound')
            continue
        detail = json.loads(row['detail_json'])
        result = dict(schema_version=1, run=run, id=identity, kind=row['kind'], phase=row['phase'], clock='driver_monotonic_ns',
                      anchor_id=anchor['id'], monotonic_ns=instant, clock_bounds_ns=bounds,
                      clock_uncertainty_ns=uncertainty, detail=detail)
        if row['phase'] == 'recovered':
            for field in ['service_progress', 'backlog_recovered']:
                source = field + '_at_us'
                if source not in detail:
                    errors.append(identity + ': missing ' + source)
                    break
                value, interval = convert(detail[source])
                result[field + '_ns'] = value
                result[field + '_bounds_ns'] = interval
            else:
                normalized.append(result)
            continue
        if row['phase'] == 'injected' and 'signal_not_before_us' in detail:
            # The controller's clock read before it signalled: the fault went
            # live between this instant and the row itself.
            value, interval = convert(detail['signal_not_before_us'])
            if not value <= instant:
                errors.append(identity + ': signal_not_before_us follows its injection row')
                continue
            result['signal_not_before_ns'] = value
            result['signal_not_before_bounds_ns'] = interval
        normalized.append(result)
    return normalized, errors


def recovery_inputs(rows, anchors, run, operations, verdict):
    normalized, errors = normalize_fault_rows(rows, anchors, run)
    if rows and not any(row['id'] == 'campaign-start' for row in anchors):
        errors.append('campaign-start: missing clock anchor')
    by_fault = defaultdict(dict)
    for row in normalized:
        if row['kind'] != 'campaign':
            by_fault[row['kind']][row['phase']] = row
    faults = []
    for key, phases in by_fault.items():
        if not {'injected', 'recovered'} <= phases.keys():
            errors.append(key + ': missing normalized injection or recovery')
            continue
        injection, recovery = phases['injected'], phases['recovered']
        upper = injection['clock_bounds_ns'][1]
        faults.append(dict(schema_version=1, id=key, clock='driver_monotonic_ns',
                           actual_ns=injection['monotonic_ns'], actual_bounds_ns=injection['clock_bounds_ns'],
                           service_progress_ns=recovery['service_progress_ns'],
                           service_progress_bounds_ns=recovery['service_progress_bounds_ns'],
                           backlog_recovered_ns=recovery['backlog_recovered_ns'],
                           backlog_recovered_bounds_ns=recovery['backlog_recovered_bounds_ns'],
                           accepted_ids=[row['id'] for row in operations if row['accepted_ns'] is not None and row['accepted_ns'] <= upper],
                           witness_verdict=verdict, anchor_ids=[injection['anchor_id'], recovery['anchor_id']]))
    return normalized, faults, errors


def recoveries(faults, operations):
    operations = {row['id']: row for row in operations}
    result = []
    for fault in faults:
        version(fault)
        require(fault['clock'] == 'driver_monotonic_ns', 'recovery requires normalized driver-clock rows')
        expected = {key for key, row in operations.items() if row['accepted_ns'] is not None and row['accepted_ns'] <= fault['actual_bounds_ns'][1]}
        require(set(fault['accepted_ids']) == expected and len(fault['accepted_ids']) == len(expected), 'pre-fault accepted population mismatch')
        require(fault['witness_verdict'] == 'passed', 'fault durability witnesses failed')
        times = [fault['actual_ns'], fault['service_progress_ns'], fault['backlog_recovered_ns']]
        require(times == sorted(times), 'fault recovery clock order')
        for key in fault['accepted_ids']:
            row = operations.get(key)
            require(row is not None and row['outcome'] in TERMINAL, 'lost pre-fault accepted input')
            require(row['observed_ns'] <= fault['backlog_recovered_bounds_ns'][1], 'backlog recovered before the terminal was observed')
        result.append({'id': fault['id'], 'service_progress_ns': times[1] - times[0],
                       'backlog_recovery_ns': times[2] - times[0],
                       'service_progress_bounds_ns': [fault['service_progress_bounds_ns'][0] - fault['actual_bounds_ns'][1],
                                                      fault['service_progress_bounds_ns'][1] - fault['actual_bounds_ns'][0]],
                       'backlog_recovery_bounds_ns': [fault['backlog_recovered_bounds_ns'][0] - fault['actual_bounds_ns'][1],
                                                     fault['backlog_recovered_bounds_ns'][1] - fault['actual_bounds_ns'][0]],
                       'anchor_ids': fault['anchor_ids']})
    return result


def recovery_report(run, operations, witness, rows, anchors):
    normalized, inputs, errors = recovery_inputs(rows, anchors, run['run'], operations, 'passed')
    if run.get('fault_campaign', False):
        if not rows:
            errors.append('L4 witness-clock faults require driver-clock recovery normalization')
        classes = witness['verdict']['classes']
        if any(name not in classes or not classes[name]['witnessed'] or classes[name]['violations']
               for name in FAULT_CLASSES):
            errors.append('fault durability witnesses failed')
    measurements = []
    if not errors:
        try:
            measurements = recoveries(inputs, operations)
        except ValueError as error:
            errors.append(str(error))
    return {'status': 'INCOMPLETE' if errors else 'COMPLETE', 'reasons': errors,
            'normalized_rows': normalized, 'inputs': inputs, 'measurements': measurements}


def reconcile_witness(run, operations, witness, evidence):
    require(len(evidence) == len(LEDGERS) and {row['ledger'] for row in evidence} == LEDGERS,
            'missing or duplicate independent witness ledger')
    for row in evidence:
        version(row)
        require(row['run'] == run['run'], 'mixed witness run identities')
    ledgers = {row['ledger']: row['rows'] for row in evidence}
    for ledger, count in [('witness_provider_receipts', 'provider_calls'),
                          ('witness_effect_attempts', 'effect_attempts'), ('witness_effect_commits', 'effect_commits')]:
        require(len(ledgers[ledger]) == witness[count], f'{ledger} counter mismatch')
    require(len(ledgers['witness_load_faults']) == witness.get('fault_rows', 0), 'fault witness counter mismatch')
    expected = {(row['subject_id'], row['scenario']): row for row in operations}
    require(len(expected) == len(operations), 'duplicate witnessed operation subject')
    for phase in ['sent', 'terminal']:
        events = [row for row in ledgers['witness_load_events'] if row['observer'] == 'driver' and row['phase'] == phase]
        require(Counter((row['subject'], row['operation']) for row in events) == Counter(expected.keys()),
                f'{phase} witness identities differ from client operations')
        for event in events:
            operation = expected[(event['subject'], event['operation'])]
            detail = json.loads(event['detail_json'])
            if phase == 'sent':
                require(detail['request'] == operation['request'], 'witness request differs from operation')
                require(int(detail['scheduled_ns']) == operation['scheduled_ns'] and int(detail['sent_ns']) == operation['sent_ns'],
                        'witness send timestamps differ from operation')
            else:
                require(int(detail['terminal_ns']) == operation['observed_ns'], 'witness terminal timestamp differs from operation')
                require(detail.get('response') == operation['response'] and detail.get('error') == operation['error'],
                        'witness terminal differs from client response')



def _protobuf_fields(payload):
    """Decode the public service-protocol fields needed by the cost receipt."""
    fields = defaultdict(list)
    position = 0
    def varint():
        nonlocal position
        value = shift = 0
        while position < len(payload) and shift < 70:
            byte = payload[position]
            position += 1
            value |= (byte & 127) << shift
            if byte < 128:
                return value
            shift += 7
        raise ValueError('invalid receipt protobuf varint')
    while position < len(payload):
        key = varint()
        number, wire = key >> 3, key & 7
        require(number > 0, 'invalid receipt protobuf field')
        if wire == 0:
            value = varint()
        elif wire == 2:
            length = varint()
            require(position + length <= len(payload), 'truncated receipt protobuf value')
            value = payload[position:position + length]
            position += length
        else:
            raise ValueError(f'unsupported receipt protobuf wire type {wire}')
        fields[number].append(value)
    return fields


def _journal_json(row):
    fields = _protobuf_fields(base64.b64decode(row['payload_base64'], validate=True))
    tag = 14 if row['entry_type'] in {'InputCommand', 'OutputCommand'} else 5
    if tag not in fields:
        return None
    content = _protobuf_fields(fields[tag][0]).get(1, [b''])[0]
    try:
        value = json.loads(content)
    except (UnicodeDecodeError, json.JSONDecodeError):
        return None
    while isinstance(value, dict) and 'body' in value:
        value = value['body']
    return value



def tool_route_census(receipt):
    """Count the native admission through owner completion and all descendants."""
    invocations = {row['id']: row for row in receipt['invocations']}
    entries = defaultdict(list)
    for row in receipt['journal']:
        entries[row['id']].append(row)
    targets = {}
    for row in receipt['journal']:
        if row['entry_type'] == 'CallInvocationIdCompletionNotification':
            fields = _protobuf_fields(base64.b64decode(row['payload_base64']))
            targets[(row['id'], fields.get(1, [0])[0])] = fields[16][0].decode()
    for row in receipt['journal']:
        if row['entry_type'] in {'CallCommand', 'OneWayCallCommand'}:
            fields = _protobuf_fields(base64.b64decode(row['payload_base64']))
            target = targets[(row['id'], fields.get(10, [0])[0])]
            require(target in invocations, 'missing called invocation descendant')
    native = []
    for identity, rows in entries.items():
        commands = {}
        for row in rows:
            if row['entry_type'] == 'RunCommand':
                fields = _protobuf_fields(base64.b64decode(row['payload_base64']))
                commands[fields.get(11, [0])[0]] = row
        for row in rows:
            if row['entry_type'] != 'RunCompletionNotification':
                continue
            value = _journal_json(row)
            if isinstance(value, dict) and isinstance(value.get('record'), dict):
                fields = _protobuf_fields(base64.b64decode(row['payload_base64']))
                command = commands.get(fields.get(1, [0])[0])
                require(command is not None, 'native Run receipt has no issuing command')
                native.append((identity, command, value['record']))
    admissions = [(identity, command) for identity, command, record in native
                  if any(event['event'] == 'admitted' for event in record['events'])]
    require(admissions, 'no native Run admission in tool cost receipt')
    owner_ids = {identity for identity, _ in admissions}
    selected_ids = set(owner_ids)
    while True:
        descendants = {row['id'] for row in invocations.values()
                       if row['invoked_by_id'] in selected_ids}
        if descendants <= selected_ids:
            break
        selected_ids |= descendants
    starts = {identity: min(command['index'] for owner, command in admissions if owner == identity)
              for identity in owner_ids}
    selected = [row for row in receipt['journal'] if row['id'] in selected_ids
                and (row['id'] not in starts or row['index'] >= starts[row['id']])]
    primitives = {'RunCommand', 'CallCommand', 'OneWayCallCommand', 'SleepCommand',
                  'AwakeableCommand', 'CompleteAwakeableCommand'}
    source = Counter(row['entry_type'] for row in selected if row['entry_type'] in primitives)
    raw = Counter(row['entry_type'] for row in selected)
    events = Counter(event['event'] for _, _, record in native for event in record['events'])
    width = receipt['fixture']['width']
    complete = receipt['branch_observation']['boundary_complete']
    if receipt['fixture']['branch'] == 'done' and complete:
        for event in ('attempt_recorded', 'decided', 'presented', 'incorporated'):
            require(events[event] == width, f'incomplete native {event} population')
    budget = 1 + 3 * width
    result = dict(boundary_complete=complete, descendant_ids=sorted(selected_ids - owner_ids),
                  native_events=dict(events), source_commands=sum(source.values()),
                  source_by_kind=dict(source), raw_engine_records=sum(raw.values()),
                  raw_by_kind=dict(raw), target_budget=budget,
                  source_target_met=sum(source.values()) <= budget,
                  raw_target_met=sum(raw.values()) <= budget,
                  boundary='native round admission through owner completion and scope close, including descendants')
    if len(owner_ids) == 1:
        result['opener'] = next(iter(owner_ids))
    return result


def tool_sql_transactions(sql):
    """Group interleaved connection-worker traces; retain every statement index."""
    opened, transactions, standalone = {}, [], []
    for index, row in enumerate(sql['rows']):
        worker = row['connection_worker']
        verb = row['sql'].lstrip().split()[0].lower()
        if verb == 'begin':
            require(worker not in opened, 'nested SQL transaction in cost trace')
            opened[worker] = [index]
        elif worker in opened:
            opened[worker].append(index)
            if verb in {'commit', 'rollback'}:
                indices = opened.pop(worker)
                templates = ' '.join(sql['rows'][i]['sql'].lower() for i in indices)
                roles = [role for role, needles in {
                    'material': ('blobs',),
                    'retention': ('referrer', 'lease', 'artifact_'),
                    'process': ('process_',),
                    'session': ('session_', 'turn_',),
                }.items() if any(needle in templates for needle in needles)]
                transactions.append(dict(connection_worker=worker, statement_indices=indices,
                                         ended=verb, table_roles=roles or ['other']))
        else:
            standalone.append(index)
    require(not opened, 'unfinished application SQL transaction in cost trace')
    require(len(transactions) == sql['application_transactions'], 'application SQL trace boundary mismatch')
    return dict(transactions=transactions, standalone_statement_indices=standalone,
                attribution='table roles overlap; these are application writes, not engine persistence')


def tool_serial_waits(invocations, complete):
    intervals = []
    for invocation in invocations:
        for start, end in invocation['sdk_input_waits']:
            require(not complete or end is not None, 'unfinished SDK wait in complete cost receipt')
            require(end is None or end >= start, 'negative SDK wait in cost receipt')
            intervals.append(dict(id=invocation['id'], start_ns=start, end_ns=end, elapsed_ns=None if end is None else end-start, censored=end is None))
    return dict(intervals=intervals,
                boundary='SDK input Pending to next read or stream end; opener intervals are serial, descendants overlap')


def tool_cost_census(receipt):
    """Reconcile a complete controlled tool receipt without conflating counts.

    The source census counts SDK run/call/send/timer/awakeable issuance. The
    raw census also includes inputs, outputs, state operations and notifications.
    HTTP requests and endpoint streams are separate transport populations.
    """
    require(receipt['contract'] == 'lash.tool-cost.controlled.v1', 'unknown tool cost contract')
    invocations = {row['id']: row for row in receipt['invocations']}
    require(len(invocations) == len(receipt['invocations']) and invocations,
            'duplicate or missing invocation identities')
    raw = Counter()
    source = Counter()
    indices = defaultdict(list)
    source_kinds = {'CallCommand', 'OneWayCallCommand', 'RunCommand', 'SleepCommand',
                    'AwakeableCommand', 'CompleteAwakeableCommand'}
    size = receipt['fixture']['payload_bytes']
    markers = {'request': b'q' * size, 'output': b'x' * size}
    decimal_markers = {name: ((str(value) + ',') * (size - 1) + str(value)).encode()
                       for name, value in [('request', 113), ('output', 120)]}
    for row in receipt['journal']:
        require(row['id'] in invocations, 'journal entry outside the captured invocation tree')
        payload = base64.b64decode(row['payload_base64'], validate=True)
        require(len(payload) == row['payload_bytes'], 'raw journal payload byte mismatch')
        for name, marker in markers.items():
            require(payload.count(marker) == row[name + '_copies'],
                    f'{name} payload copy mismatch')
        for name, marker in decimal_markers.items():
            require(payload.count(marker) == row[name + '_decimal_copies'], f'{name} decimal copy mismatch')
        indices[row['id']].append(row['index'])
        raw[row['entry_type']] += 1
        if row['entry_type'] in source_kinds:
            source[row['entry_type']] += 1
    for identity, invocation in invocations.items():
        require(sorted(indices[identity]) == list(range(invocation['journal_size'])),
                f'incomplete raw journal for {identity}')
        parent = invocation['invoked_by_id']
        require(parent is None or parent in invocations, 'missing invocation ancestor')
        require(not receipt['branch_observation']['boundary_complete'] or invocation['status'] == 'completed', 'captured an unfinished invocation')
    for name, counts in [('engine', raw), ('source', source)]:
        require(dict(counts) == receipt[name]['by_kind'], f'{name} per-kind census mismatch')
        require(sum(counts.values()) == receipt[name]['total'], f'{name} total census mismatch')
    sql = receipt['sql']
    require(sum(sql['by_verb'].values()) == sql['statements'] == len(sql['rows']),
            'SQL trace population mismatch')
    require(sql['application_transactions'] == sql['by_verb']['begin'], 'SQL transaction count mismatch')
    require(sql['expanded_statement_bytes'] == sum(row['expanded_bytes'] for row in sql['rows']),
            'SQL byte census mismatch')
    require(receipt['bytes']['journal_protobuf_payload'] == sum(row['payload_bytes'] for row in receipt['journal']),
            'journal byte census mismatch')
    require(receipt['bytes']['endpoint_request_framed'] == sum(row['endpoint_request_bytes'] for row in invocations.values()),
            'endpoint request byte census mismatch')
    require(receipt['bytes']['endpoint_response_framed'] == sum(frame[1] for row in invocations.values() for frame in row['endpoint_response_frames']),
            'endpoint response byte census mismatch')
    byte_services = {}
    for row in receipt['journal']:
        service = invocations[row['id']]['target_service_name']
        subtotal = byte_services.setdefault(service, Counter())
        subtotal['protobuf_payload_bytes'] += row['payload_bytes']
        for role in ('request', 'output'):
            subtotal[role + '_payload_occurrences'] += row[role + '_copies']
            subtotal[role + '_decimal_occurrences'] += row[role + '_decimal_copies']
    for role in ('request', 'output'):
        for encoding in ('payload', 'decimal'):
            key = role + '_' + encoding + '_occurrences'
            require(sum(row[key] for row in byte_services.values()) == receipt['bytes'][key],
                    'aggregate material copy census mismatch')
    route = tool_route_census(receipt)
    waits = tool_serial_waits(receipt['invocations'], receipt['branch_observation']['boundary_complete'])
    if 'opener' in route:
        waits['opener_serial_wait_ns'] = sum(row['elapsed_ns'] or 0 for row in waits['intervals'] if row['id'] == route['opener'])
        waits['opener_serial_waits'] = sum(row['id'] == route['opener'] for row in waits['intervals'])
    if not receipt['branch_observation']['boundary_complete']:
        refusals = receipt['branch_observation']['codec_refusals']
        require(receipt['fixture']['payload_bytes'] >= 1_000_000 and refusals, 'unexplained incomplete cost boundary')
        for refusal in refusals:
            require(refusal['id'] in invocations and refusal['code'] == 400
                    and 'JSON decode nodes limit 1000000 exceeded by 1000001' in refusal['message'],
                    'codec refusal identity or cause mismatch')
            failures = [row for row in receipt['journal'] if row['id'] == refusal['id'] and row['entry_type'] == 'OutputCommand']
            require(len(failures) == 1, 'missing raw codec refusal')
            failure = _protobuf_fields(_protobuf_fields(base64.b64decode(failures[0]['payload_base64']))[15][0])
            require(failure[1][0] == refusal['code'] and failure[2][0].decode() == refusal['message'], 'raw codec refusal differs from reported cause')
    return {'byte_services': byte_services, 'boundary_complete': receipt['branch_observation']['boundary_complete'], 'codec_refusals': receipt['branch_observation']['codec_refusals'], 'tool_route': route, 'sql_trace': tool_sql_transactions(sql), 'sdk_waits': waits,
            'source_commands': sum(source.values()), 'raw_engine_records': sum(raw.values()),
            'invocations': len(invocations), 'application_transactions': sql['application_transactions'],
            'historical_source_hypothesis': receipt['branch_observation']['historical_source_hypothesis'],
            'target_source_budget': receipt['branch_observation']['target_source_budget']}


def cancellation_census(samples, owned, disappeared):
    available = all('cancellation_commands' in sample for sample in samples)
    commands = {}
    targets = {}
    for sample in samples:
        for row in sample.get('cancellation_commands', []):
            require(row['id'] in owned, 'unowned cancellation sender')
            identity = (row['id'], row['index'])
            require(identity not in commands or commands[identity] == row, 'journal cancellation command changed')
            commands[identity] = row
            if row['version'] != 2:
                available = False  # v1 lacks the decoded entry_json projection.
                continue
            require(row['entry_type'] == 'Command: SendSignal', 'unsupported cancellation command')
            try:
                command = json.loads(row['entry_json'])['Command']['SendSignal']
                require(isinstance(command, dict) and {'signal_id', 'result', 'target_invocation_id'} <= command.keys(),
                        'incomplete cancellation journal projection')
            except (KeyError, TypeError, ValueError) as error:
                raise ValueError('invalid cancellation journal projection') from error
            if command['signal_id'] == {'Index': 1}:
                require(command['result'] == 'Void', 'invalid built-in cancellation payload')
                require(isinstance(command['target_invocation_id'], str), 'missing cancellation target invocation ID')
                targets.setdefault(command['target_invocation_id'], []).append(identity)
    explained = [row['id'] for row in disappeared if row['status'] == 'inboxed'
                 and row['id'] in targets]
    return {'available': available, 'commands': len(commands), 'target_invocation_ids': sorted(targets),
            'explained_inbox_ids': sorted(explained),
            'unexplained_disappearance_ids': sorted(row['id'] for row in disappeared if row['id'] not in explained)}


def collection_gaps(run, sample_errors, normalized_faults):
    """Attribute only periodic gaps definitely inside a matching fault.

    A failed observation happened at its instant, which must lie in the window.
    The window opens once the controller's clock read before its signal has
    certainly happened, when the injection records one, and otherwise once the
    injection row certainly exists.
    A counter epoch transition happened at an unknown point after the last
    old-epoch observation and no later than the first new-epoch one; that
    interval must meet the window, so a reset first seen after recovery is
    still the fault's, and one wholly before or after it is not."""
    phases = defaultdict(dict)
    for row in normalized_faults:
        if row['kind'] != 'campaign':
            phases[row['kind']][row['phase']] = row
    windows = []
    for kind, rows in phases.items():
        if not {'injected', 'recovered'} <= rows.keys():
            continue
        injection, recovery = rows['injected'], rows['recovered']
        # Use the inner bounds so clock uncertainty cannot excuse an outside gap.
        opened = injection.get('signal_not_before_bounds_ns', injection['clock_bounds_ns'])
        start, end = opened[1], recovery['clock_bounds_ns'][0]
        component = {'worker-kill': 'worker', 'restate-restart': 'restate',
                     'rolling-deploy': 'worker'}.get(injection['kind'])
        targets = injection['detail'].get('collection_targets', [])
        targets = [target for target in targets if target.get('component') == component]
        if start <= end and component is not None:
            windows.append(dict(kind=kind, window_ns=[start, end], targets=targets,
                                anchor_ids=[injection['anchor_id'], recovery['anchor_id']]))
    gaps = []
    for row in sample_errors:
        version(row)
        require(row['run'] == run['run'], 'mixed sample error run identities')
        require(type(row['monotonic_ns']) is int and row['monotonic_ns'] >= 0,
                'invalid sample error timestamp')
        if row['record'] == 'counter_gap':
            require(type(row['previous_observed_ns']) is int
                    and 0 <= row['previous_observed_ns'] < row['monotonic_ns'],
                    'invalid counter gap interval')
            inside = lambda start, end: row['previous_observed_ns'] < end and start <= row['monotonic_ns']
        else:
            inside = lambda start, end: start <= row['monotonic_ns'] <= end
        matches = [window for window in windows
                   if run.get('fault_campaign', False) and row.get('sample_kind') == 'periodic'
                   and inside(*window['window_ns'])
                   and row.get('target') in window['targets']]
        attribution = {'status': 'UNATTRIBUTED'}
        if len(matches) == 1:
            attribution = {key: value for key, value in matches[0].items() if key != 'targets'}
            attribution['status'] = 'FAULT_ATTRIBUTED'
        gaps.append({**row, 'attribution': attribution})
    return gaps


class CounterEpochGapError(ValueError):
    def __init__(self, gaps):
        super().__init__('counter epoch gap: incomplete measurement')
        self.collection_gaps = gaps


def summarize(run, operations, samples, witness, faults=(), sample_errors=(), witness_evidence=None, anchors=()):
    for row in [run, witness, *samples]:
        version(row)
        require(row['run'] == run['run'], 'mixed run identities')
    require(all(row['run'] == run['run'] for row in operations), 'mixed operation run identities')
    if witness_evidence is not None:
        reconcile_witness(run, operations, witness, witness_evidence)
    count = populations(operations)
    require(count['offered'] == witness['sent'] == witness['terminal'], 'witness population mismatch')
    classes = witness['verdict']['classes']
    campaign = run.get('fault_campaign', False)
    require(set(classes) == set(WITNESS_CLASSES) | (set(FAULT_CLASSES) if campaign else set()), 'missing or unknown durable evidence classes')
    require(classes and all(row['witnessed'] > 0 and not row['violations'] for row in classes.values()), 'durability witness failed')
    primary = [row for row in operations if row['scenario'] == 'turn']
    planned = run['sessions'] * run['turns_per_session']
    require(len(primary) >= planned if campaign else len(primary) == planned, 'primary offered population mismatch')
    if campaign:
        actors = defaultdict(list)
        for row in primary:
            actors[row['actor_id']].append(row['request']['ordinal'])
        require(set(actors) == set(range(run['sessions'])), 'campaign actor population mismatch')
        require(all(len(ordinals) >= run['turns_per_session'] and sorted(ordinals) == list(range(len(ordinals)))
                    for ordinals in actors.values()), 'campaign primary ordinals are incomplete')
    require(len(samples) >= 2, 'missing initial/final metric sample')
    samples = sorted(samples, key=lambda row: row['monotonic_ns'])
    require(samples[0]['monotonic_ns'] <= min(row['sent_ns'] for row in operations), 'metrics began after load')
    require(samples[-1]['monotonic_ns'] >= max(row['observed_ns'] for row in operations), 'metrics ended before drain')
    groups = defaultdict(list)
    for row in operations:
        groups[(row['scenario'], row['phase'])].append(row)
    histograms = []
    for (scenario, phase), rows in sorted(groups.items()):
        end = max(row['observed_ns'] for row in rows)
        start = min(row['scheduled_ns'] for row in rows)
        duration = (end - start) / 1e9
        population = populations(rows)
        timelines = sorted([(row['scheduled_ns'], 1) for row in rows] +
                           [(row['observed_ns'], -1) for row in rows if row['outcome'] in TERMINAL])
        backlog = peak = 0
        for _, delta in timelines:
            backlog += delta
            peak = max(peak, backlog)
        histograms.append({'schema_version': 1, 'scenario': scenario, 'phase': phase, 'population': population,
                           'scheduled_to_terminal_ns': distribution([row['observed_ns'] - row['scheduled_ns'] for row in rows if row['outcome'] in TERMINAL]),
                           'send_to_terminal_ns': distribution([row['observed_ns'] - row['sent_ns'] for row in rows if row['outcome'] in TERMINAL]),
                           'queue_delay_ns': distribution([row['sent_ns'] - row['scheduled_ns'] for row in rows]),
                           'duration_s': duration, 'offered_per_s': len(rows) / duration if duration else None,
                           'completed_per_s': population['durable_terminal'] / duration if duration else None,
                           'backlog_peak': peak, 'backlog_at_end': backlog,
                           'latency_by_outcome_ns': {outcome: distribution([row['observed_ns'] - row['scheduled_ns'] for row in rows if row['outcome'] == outcome]) for outcome in population['outcomes']}})
    counters = defaultdict(CounterDeltas)
    counter_gaps = []
    exporter_gaps = []
    observations = {}

    def observe_counter(name, identity, epoch, value, observed_ns, target):
        counter = counters[name]
        previous_epoch = counter.epochs.get(identity)
        if previous_epoch is not None and previous_epoch != epoch:
            counter_gaps.append(dict(schema_version=1, record='counter_gap', run=run['run'],
                                     sample_kind='periodic', monotonic_ns=observed_ns,
                                     previous_observed_ns=observations[(name, identity)],
                                     target=target, counter=name, identity=identity, complete=False,
                                     before={'epoch': previous_epoch, 'value': counter.values[(identity, previous_epoch)]},
                                     after={'epoch': epoch, 'value': value}, unobserved_delta=None))
        counter.add(identity, epoch, value)
        observations[(name, identity)] = observed_ns

    worker_peaks = {}
    memory_peaks = []
    journal_rows = []
    owned_invocations = {}
    invocation_states = {}
    parked_transitions = 0
    parked_ns = 0
    last_time = samples[0]['monotonic_ns']
    epoch_rows = []
    physical_peaks = {}
    snapshot_peak = 0
    initial_invocations = {row['id'] for row in samples[0]['invocations']}
    baseline_nodes = {row['address'].rstrip('/'): row['gen_node_id'] for row in samples[0]['node_epochs']}
    for sample_index, sample in enumerate(samples):
        require(sample['collection_finished_ns'] >= sample['monotonic_ns'], 'collector clock order')
        worker_resources = []
        require(len(sample['workers']) == len(run['workers']), 'missing worker resource sample')
        require({row['node'] for row in sample['workers']} == set(run['workers']), 'missing or duplicate worker sample')
        for worker in sample['workers']:
            node, resources = worker['node'], worker['resources']
            validate_resources(resources)
            worker_resources.append(resources)
            worker_peaks[node] = max(worker_peaks.get(node, 0), resources['cgroup_peak_bytes'])
            epoch = next(row['epoch'] for row in resources['processes'] if row['parent_pid'] not in {proc['pid'] for proc in resources['processes']})
            for name in ('usage_usec', 'throttled_usec', 'nr_throttled'):
                observe_counter('worker_' + name, node, epoch, resources['cgroup_cpu'][name],
                                worker['observed_ns'], {'component': 'worker', 'endpoint': node})
            observe_counter('worker_ooms', node, epoch, resources['cgroup_events']['oom_kill'],
                            worker['observed_ns'], {'component': 'worker', 'endpoint': node})
        memory_peaks.append(deployment_memory(worker_resources))
        database = sample['postgres']
        for name in ('transactions', 'blocks_read', 'blocks_hit', 'read_ms', 'write_ms', 'wal_bytes', 'query_calls', 'query_ms'):
            reset = database['wal_reset'] if name == 'wal_bytes' else database['query_reset'] if name.startswith('query_') else database['stats_reset']
            observe_counter('postgres_' + name, 'postgres', (database['epoch'], reset), database[name],
                            sample['collection_finished_ns'], {'component': 'postgres', 'endpoint': 'lash_database'})
        for invocation in sample['invocations']:
            key = invocation.get('target_service_key') or ''
            if invocation["id"] not in initial_invocations or f"load-{run['run']}-" in key or invocation.get('invoked_by_id') in owned_invocations:
                owned_invocations[invocation['id']] = invocation
        # Repeat to include descendants regardless of SQL scan ordering.
        while True:
            before = len(owned_invocations)
            for invocation in sample['invocations']:
                if invocation.get('invoked_by_id') in owned_invocations:
                    owned_invocations[invocation['id']] = invocation
            if len(owned_invocations) == before:
                break
        parked_ns += sum(status == 'suspended' for status in invocation_states.values()) * (sample['monotonic_ns'] - last_time)
        for invocation in sample['invocations']:
            if invocation['id'] in owned_invocations:
                status = invocation['status']
                parked_transitions += status == 'suspended' and invocation_states.get(invocation['id']) != 'suspended'
                invocation_states[invocation['id']] = status
        last_time = sample['monotonic_ns']
        journal_rows.extend(row for row in sample['journals'] if row['id'] in owned_invocations)
        require(len(sample['restate']) == 3, 'missing Restate node sample')
        for node in sample['restate']:
            metadata = next((row for row in sample['node_epochs'] if row['address'].rstrip('/') == node['node'].rstrip('/')), None)
            require(metadata is not None, 'Restate metrics have no node epoch')
            node_epoch = metadata['gen_node_id']
            metrics = prometheus(node['prometheus'])
            if not any(name in {'restate_invoker_invocation_tasks_total', 'restate_num_active_partitions'} for name, _, _ in metrics):
                # A restarted node answers its scrape before its partition
                # processors publish these series. The scrape is a gap at its
                # instant, the node's counters untouched: a fault on that
                # node may explain it, and the first and last samples never.
                interior = 0 < sample_index < len(samples) - 1
                exporter_gaps.append(dict(schema_version=1, record='sample_error', run=run['run'],
                                          sample_kind='periodic' if interior else 'final',
                                          monotonic_ns=node['metrics_observed_ns'], error=MISSING_INVOKER_EXPORTER,
                                          target={'component': 'restate', 'endpoint': node['node']}))
                metrics = []
            for name, labels, value in metrics:
                if name == 'restate_invoker_invocation_tasks_total':
                    partition = labels['partition_id']
                    leader = next((row for row in sample['leader_epochs'] if str(row['partition_id']) == partition), None)
                    require(leader is not None and leader['leader_epoch'] is not None, 'retry counter has no leader epoch')
                    epoch_rows.append({'node': node['node'], 'node_epoch': node_epoch, 'partition': partition,
                                       'leader': leader['leader_gen_node_id'], 'leader_epoch': leader['leader_epoch']})
                    # Node counters survive leadership changes. Include leader epochs
                    # in evidence, but do not discard cumulative node deltas at handoff.
                    identity = (node['node'], tuple(sorted(labels.items())))
                    counter_name = 'restate_task_' + labels['status']
                    if labels['status'] == 'failed':
                        counter_name += '_retryable' if labels.get('transient') == 'true' else '_terminal'
                    counter = counters[counter_name]
                    if (sample_index > 0 and identity not in counter.epochs
                            and baseline_nodes[node['node'].rstrip('/')] == node_epoch):
                        # A counter series absent from the initial scrape was
                        # zero. Preserve its first event instead of rebasing it.
                        counter.add(identity, node_epoch, 0)
                    observe_counter(counter_name, identity, node_epoch, value, node['metrics_observed_ns'],
                                    {'component': 'restate', 'endpoint': node['node']})
            snapshot = node['physical']['snapshot']
            require(snapshot is not None, 'missing remote snapshot bytes')
            snapshot_peak = max(snapshot_peak, snapshot['bytes'])
            physical_peaks[node['node']] = max(physical_peaks.get(node['node'], 0), node['physical']['allocated_bytes'])
    journal_rows.extend({'id': row['invocation_id'], 'journal': row['journal']} for row in operations if row['journal'] is not None)
    for row in operations:
        require(row['accepted_ns'] is None or row['invocation_id'] in owned_invocations, 'accepted invocation missing from final journal census')
    final_invocations = {row['id'] for row in samples[-1]['invocations']}
    disappeared = [row for key, row in owned_invocations.items() if key not in final_invocations]
    cancellations = cancellation_census(samples, owned_invocations, disappeared)
    unexplained = [row for row in disappeared if row['id'] in cancellations['unexplained_disappearance_ids']]
    unfinished = [row for key, row in owned_invocations.items() if key in final_invocations and invocation_states.get(key) != 'completed']
    require(counters['restate_task_started'].total >= count['accepted'], 'Restate task counters omit accepted operations')
    for name in ['restate_task_failed_retryable', 'restate_task_failed_terminal', 'restate_task_suspended']:
        counters[name]  # A supported series absent throughout the run is zero.
    delta_summary = {name: {'observed_delta': value.total, 'epoch_gaps': value.gaps,
                            'complete': value.gaps == 0} for name, value in sorted(counters.items())}
    recovery = recovery_report(run, operations, witness, faults, anchors)
    pool = pool_report(run, operations, samples)
    gaps = collection_gaps(run, [*sample_errors, *exporter_gaps, *counter_gaps, *pool.pop('epoch_gaps')],
                           recovery['normalized_rows'])
    pool_gaps = [row for row in gaps if row.get('counter') == 'pool_identity']
    epoch_gaps = [row for row in gaps if row['record'] == 'counter_gap' and row not in pool_gaps]
    if any(row['attribution']['status'] == 'UNATTRIBUTED' for row in epoch_gaps):
        raise CounterEpochGapError(gaps)
    for name, counter in delta_summary.items():
        counter['fault_attributed_epoch_gaps'] = sum(row['counter'] == name for row in epoch_gaps)
    require(witness['effect_attempts'] - witness['effect_commits'] == witness['verdict']['absorbed_effect_attempts'], 'absorbed effect attempt mismatch')
    require(0 <= witness['provider_retryable_failures'] <= witness['provider_calls'], 'provider receipt population mismatch')
    require(witness['effect_attempts'] >= witness['effect_commits'], 'effect attempts smaller than commits')
    require(count['unresolved'] == 0, 'unresolved durable operation population')
    completed = count['durable_terminal']
    journal = journal_summary(journal_rows, completed)
    missing_journals = [key for key in owned_invocations if key not in cancellations['explained_inbox_ids']
                        and (key not in journal['maxima'] or journal['maxima'][key]['entries'] == 0)]
    inputs = recovery['inputs']
    # A pool restart follows the counter epoch rule: the fault on its worker
    # explains it, with the unobserved counters left unknown. One that no
    # single fault explains leaves the pool, and so the run, incomplete.
    pool_attributed = sum(row['attribution']['status'] == 'FAULT_ATTRIBUTED' for row in pool_gaps)
    if pool_attributed != len(pool_gaps):
        pool = {**pool, 'status': 'INCOMPLETE', 'reasons': [POOL_EPOCH_CHANGED, *pool['reasons']], 'executions': []}
    pool.update(epoch_gaps=len(pool_gaps), fault_attributed_epoch_gaps=pool_attributed, complete=not pool_gaps)
    unattributed = any(row['attribution']['status'] == 'UNATTRIBUTED' for row in gaps if row not in pool_gaps)
    reasons = recovery['reasons'][:] + pool['reasons']
    if any(row.get('error') == MISSING_INVOKER_EXPORTER and row['attribution']['status'] == 'UNATTRIBUTED' for row in gaps):
        reasons.append(MISSING_INVOKER_EXPORTER)
    if unattributed:
        reasons.append('required collection intervals are missing')
    if unfinished:
        reasons.append('run-owned internal invocations remain unfinished')
    if unexplained:
        reasons.append('run-owned invocations disappeared without a recorded inbox cancellation')
    if missing_journals:
        reasons.append('run-owned invocation has an empty, missing or pruned journal')
    status = 'FAILED' if unattributed or (unexplained and cancellations['available']) else 'INCOMPLETE' if reasons else 'PASSED'
    qualification = {'status': status, 'reasons': reasons,
                     'unfinished_internal': unfinished, 'disappeared_invocations': unexplained,
                     'missing_journal_ids': missing_journals}
    normalized = {name: row['observed_delta'] / completed if completed else None
                  for name, row in delta_summary.items() if name.startswith('postgres_')}
    return {'schema_version': 1, 'run': run['run'], 'mode': run.get('mode', 'smoke'), 'population': count,
            'definition_hash': run['definition_hash'], 'target': run['target'],
            'qualification': qualification, 'collection_gaps': gaps,
            'cancellations': cancellations,
            'histograms': histograms, 'counters': delta_summary, 'retry_epochs': epoch_rows,
            'client_retries': sum(row['client_attempts'] - 1 for row in operations),
            'client_reattaches': sum(row.get('client_reattaches', 0) for row in operations),
            'provider_calls': witness['provider_calls'],
            'provider_retryable_failures': witness['provider_retryable_failures'], 'effect_attempts': witness['effect_attempts'],
            'effect_commits': witness['effect_commits'],
            'absorbed_effect_attempts': witness['effect_attempts'] - witness['effect_commits'],
            'journals': journal, 'postgres_per_terminal': normalized,
            'postgres_peaks': {key: max(row['postgres'][key] for row in samples) for key in ('connections', 'waiters', 'lock_waiters')},
            'deployment_memory_peak_bytes': max(memory_peaks), 'worker_peak_bytes': worker_peaks,
            'physical_peak_bytes_by_node': physical_peaks, 'shared_snapshot_peak_bytes': snapshot_peak,
            'parks': {'sampled_transitions': parked_transitions, 'sampled_occupancy_ns': parked_ns,
                      'resolution': 'collector intervals; sub-interval parks are not observable'},
            'pool': pool,
            'unavailable': {'postgres_lock_wait_duration': 'PostgreSQL provides current waiters, not cumulative per-lock time',
                            'remote_clock_offsets': 'no synchronized remote clock authority'},
            'recoveries': recovery['measurements'], 'faults_exercised': len(inputs),
            'recovery_inputs': recovery,
            'fault_clock': 'L4 raw fault rows use the independent PostgreSQL witness clock; normalized recovery inputs use the driver monotonic clock',
            'saturation': 'NOT_RUN', 'baseline': None, 'budgets': None,
            'invariants': {'witness_populations': 'passed', 'resource_totals': 'passed',
                           'retry_epochs': 'passed', 'completed_journals': 'passed' if not reasons else 'INCOMPLETE'}}


def metric_rows(samples):
    """Explicit units and identities; raw collector records remain in samples.jsonl."""
    for sample in samples:
        def metric(node, deployment, generation, name, unit, value, **labels):
            return dict(schema_version=1, run=sample['run'], monotonic_ns=sample['monotonic_ns'],
                        collection_finished_ns=sample['collection_finished_ns'],
                        node=node, deployment=deployment, generation=generation,
                        metric=name, unit=unit, value=value, labels=labels,
                        clock_uncertainty_ns=sample['collection_finished_ns'] - sample['monotonic_ns'])
        for worker in sample['workers']:
            node, data = worker['node'], worker['resources']
            generation = data.get('generation')
            pool = data.get('pool')
            if isinstance(pool, dict):
                for name, unit in {**POOL_GAUGE_UNITS, **POOL_COUNTER_UNITS}.items():
                    value = pool.get(name) if name in POOL_GAUGE_UNITS else pool.get('counters', {}).get(name)
                    if value is not None:
                        yield metric(node, node, generation, 'pool_' + name, unit, value, pool_epoch=pool.get('epoch'),
                                     parent_epoch=next((row['epoch'] for row in data['processes'] if row['parent_pid'] not in {proc['pid'] for proc in data['processes']}), None))
            for name in ['cgroup_memory_bytes', 'cgroup_peak_bytes', 'rss_sum_bytes']:
                yield metric(node, node, generation, name, 'bytes', data[name])
            for proc in data['processes']:
                yield metric(node, node, generation, 'process_rss', 'bytes', proc['rss_bytes'], pid=proc['pid'], epoch=proc['epoch'])
                yield metric(node, node, generation, 'process_cpu', 'seconds', proc['cpu_ticks'] / data['clock_ticks_per_second'], pid=proc['pid'], epoch=proc['epoch'])
            for name, value in data['cgroup_cpu'].items():
                yield metric(node, node, generation, 'cgroup_cpu_' + name, 'microseconds' if name.endswith('_usec') else 'count', value)
            for name, value in data['cgroup_events'].items():
                yield metric(node, node, generation, 'cgroup_memory_' + name, 'count', value)
        database = sample['postgres']
        for name, value in database.items():
            if isinstance(value, (int, float)):
                unit = 'bytes' if name == 'wal_bytes' else 'milliseconds' if name.endswith('_ms') else 'count'
                yield metric('postgres', 'postgres', database['epoch'], name, unit, value, scope='Lash database' if name != 'wal_bytes' else 'server including witness database')
        for row in sample['journals']:
            for name, unit in [('entries', 'count'), ('bytes', 'bytes')]:
                yield metric('restate', row['id'], None, 'journal_' + name, unit, row['journal'][name])
        for node in sample['restate']:
            epoch = next(row['gen_node_id'] for row in sample['node_epochs'] if row['address'].rstrip('/') == node['node'].rstrip('/'))
            yield metric(node['node'], node['node'], epoch, 'physical_allocated', 'bytes', node['physical']['allocated_bytes'])
            for directory, value in node['physical']['directories'].items():
                yield metric(node['node'], node['node'], epoch, 'physical_directory', 'bytes', value, directory=directory)
            for name, labels, value in prometheus(node['prometheus']):
                if name == 'restate_invoker_invocation_tasks_total':
                    yield metric(node['node'], node['node'], epoch, name, 'count', value, **labels)
        # The snapshot prefix is shared by every Restate node.
        yield metric('s3', 'snapshot-prefix', None, 'snapshot_objects_bytes', 'bytes', max(node['physical']['snapshot']['bytes'] for node in sample['restate']))


def read_records(path):
    records = []
    for line in path.read_text().splitlines():
        if line.startswith('load measurement '):
            row = json.loads(line[len('load measurement '):])
            version(row)
            require(row['record'] in RECORDS, 'unsupported measurement record')
            records.append(row)
    return records


def archive(log, output, recovery_only=False):
    records = read_records(log)
    runs = [row for row in records if row['record'] == 'run']
    witnesses = [row for row in records if row['record'] == 'witness']
    require(len(runs) == len(witnesses) == 1, 'missing or duplicate run/witness record')
    identity = {key: runs[0][key] for key in ('definition_hash', 'target')}
    require(all(row['run'] == runs[0]['run'] for row in records), 'mixed archive run identities')
    operations = [row for row in records if row['record'] == 'operation']
    samples = [row for row in records if row['record'] == 'sample']
    anchors = [row for row in records if row['record'] == 'clock_anchor']
    sample_errors = [row for row in records if row['record'] == 'sample_error']
    query_retries = [row for row in records if row['record'] == 'query_retry']
    witness_evidence = [row for row in records if row['record'] == 'witness_evidence']
    faults = [dict(schema_version=1, **row) for row in
              next((row['rows'] for row in witness_evidence if row['ledger'] == 'witness_load_faults'), [])]
    output = output / 'fig-3790' / runs[0]['run']
    output.mkdir(parents=True, exist_ok=True)
    for name, rows in [('operations', operations), ('samples', samples), ('sample_errors', sample_errors),
                       ('query_retries', query_retries), ('witness_evidence', witness_evidence), ('faults', faults), ('clock_anchors', anchors)]:
        (output / (name + '.jsonl')).write_text(''.join(json.dumps(row, sort_keys=True) + '\n' for row in rows))
    (output / 'witness.json').write_text(json.dumps(witnesses[0], indent=2) + '\n')
    (output / 'collection.json').write_text(json.dumps({**runs[0], 'source_log_sha256': hashlib.sha256(log.read_bytes()).hexdigest(),
                                                     'scope': 'pipeline smoke; no baseline or budgets'}, indent=2) + '\n')
    (output / 'pool_executions.jsonl').write_text('')
    # Recovery collection has its own qualification. Preserve its evidence even
    # when another measurement (for example restart counter gaps) is incomplete.
    reconcile_witness(runs[0], operations, witnesses[0], witness_evidence)
    recovery = recovery_report(runs[0], operations, witnesses[0], faults, anchors)
    (output / 'recovery.json').write_text(json.dumps({'schema_version': 1, 'run': runs[0]['run'], **recovery}, indent=2) + '\n')
    (output / 'recovery_inputs.jsonl').write_text(''.join(json.dumps(row, sort_keys=True) + '\n' for row in recovery['inputs']))
    print(f"load recovery inputs={recovery['status']} faults={len(recovery['inputs'])} normalized_rows={len(recovery['normalized_rows'])} results={output / 'recovery.json'}")
    (output / 'normalized_faults.jsonl').write_text(''.join(json.dumps(row, sort_keys=True) + '\n' for row in recovery['normalized_rows']))
    gaps = collection_gaps(runs[0], sample_errors, recovery['normalized_rows'])
    (output / 'collection_gaps.jsonl').write_text(''.join(json.dumps(row, sort_keys=True) + '\n' for row in gaps))
    if recovery_only:
        (output / 'summary.json').write_text(json.dumps({'schema_version': 1, 'run': runs[0]['run'],
                                                       **identity,
                                                       'verdict': 'INCOMPLETE', 'error': 'final metric census pending'}) + '\n')
        (output / 'histograms.json').write_text(json.dumps({'schema_version': 1, 'status': 'INCOMPLETE', 'histograms': []}) + '\n')
        (output / 'metrics.jsonl').write_text(''.join(json.dumps(row, sort_keys=True) + '\n' for row in metric_rows(samples)))
        require(recovery['status'] == 'COMPLETE', 'recovery qualification is INCOMPLETE: ' + '; '.join(recovery['reasons']))
        return
    try:
        summary = summarize(runs[0], operations, samples, witnesses[0], faults, sample_errors, witness_evidence, anchors)
    except ValueError as error:
        if isinstance(error, CounterEpochGapError):
            gaps = error.collection_gaps
            (output / 'collection_gaps.jsonl').write_text(''.join(json.dumps(row, sort_keys=True) + '\n' for row in gaps))
        (output / 'summary.json').write_text(json.dumps({'schema_version': 1, **identity, 'verdict': 'failed', 'error': str(error), 'collection_gaps': gaps}) + '\n')
        raise
    (output / 'collection_gaps.jsonl').write_text(''.join(json.dumps(row, sort_keys=True) + '\n' for row in summary['collection_gaps']))
    (output / 'metrics.jsonl').write_text(''.join(json.dumps(row, sort_keys=True) + '\n' for row in metric_rows(samples)))
    (output / 'histograms.json').write_text(json.dumps({'schema_version': 1, 'histograms': summary.pop('histograms')}, indent=2) + '\n')
    (output / 'pool_executions.jsonl').write_text(''.join(json.dumps(dict(schema_version=1, run=runs[0]['run'], **row), sort_keys=True) + '\n' for row in summary['pool']['executions']))
    (output / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    qualification = summary['qualification']['status']
    print(f"load measurements qualification={qualification} operations={len(operations)} samples={len(samples)} journals={summary['journals']['invocations']} counter_series={len(summary['counters'])} recovery_inputs={summary['recovery_inputs']['status']} faults={summary['faults_exercised']} pool={summary['pool']['status']} results={output}")
    require(qualification == 'PASSED', f'load qualification is {qualification}: ' + '; '.join(summary['qualification']['reasons']))


if __name__ == '__main__':
    parser = argparse.ArgumentParser()
    parser.add_argument('log', type=Path)
    parser.add_argument('output', type=Path)
    parser.add_argument('--recovery-only', action='store_true')
    args = parser.parse_args()
    archive(args.log, args.output, args.recovery_only)
