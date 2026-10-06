#!/usr/bin/env python3
"""Archive completed live baseline batches and render their measured tables.

Use --inputs with the directories written by restate-baseline.py to collect
completed batches (including batches retained before a later fixture failed).
Use --ledger to reproduce the Markdown tables from the committed ledger.
"""
import argparse
from collections import defaultdict
import json
import math
from pathlib import Path


def collect(inputs):
    runs = []
    for directory in inputs:
        payload = json.loads((directory / 'samples.json').read_text())
        machine = json.loads((directory / 'machine.json').read_text())
        cpu = machine.pop('cpu')
        memory = machine.pop('memory')
        machine['cpu_model'] = next(line.split(':', 1)[1].strip() for line in cpu.splitlines() if line.startswith('model name'))
        machine['logical_cpus'] = sum(line.startswith('processor') for line in cpu.splitlines())
        machine['memory_total_kib'] = int(memory.splitlines()[0].split()[1])
        batches = payload['batches']
        for batch in batches:
            # Keep the observed journal bounds without archiving the same
            # historical invocation rows in every later batch.
            rows = batch.pop('invocations')
            batch['journal_observed_end'] = sum(row['journal_size'] for row in rows)
            batch['journal_observed_start'] = batch['journal_observed_end'] - batch['journal_entries']
            batch.setdefault('pool_max_connections', 32)
        runs.append({
            'artifact_directory': str(directory), 'machine': machine,
            'command': json.loads((directory / 'command.json').read_text()), 'batches': batches,
        })
    return {'runs': runs}


def percentile(values, fraction):
    ordered = sorted(values)
    return ordered[max(0, math.ceil(len(ordered) * fraction) - 1)]


def render(ledger):
    grouped = defaultdict(list)
    for run in ledger['runs']:
        for batch in run['batches']:
            case = batch['scenario']
            grouped[(case['name'], case['rounds'], case['tools_per_round'], case['sessions'])].append(batch)
    turns = []
    writes = []
    suspensions = []
    concurrency = []
    for (name, rounds, tools, sessions), batches in grouped.items():
        samples = [sample for batch in batches for sample in batch['samples']]
        totals = [sample['send_to_completion_ms'] for sample in samples]
        intervals = [value for sample in samples for value in sample['round_ms']]
        sql = sum(batch['sql_writes']['statements'] for batch in batches) / len(samples)
        journal = sum(batch['journal_entries'] for batch in batches) / len(samples)
        if name == 'rounds':
            turns.append(f'| {rounds} | {len(samples)} | {percentile(intervals, .5):.3f} | {percentile(intervals, .99):.3f} | {percentile(totals, .5):.3f} | {percentile(totals, .99):.3f} |')
            writes.append(f'| {rounds} | {sql:.2f} | {journal:.2f} | {sql / rounds:.2f} | {journal / rounds:.2f} | {(sql + journal) / rounds:.2f} |')
        elif name == 'concurrent':
            concurrency.append(f'| {sessions} | {batches[0]["pool_max_connections"]} | {len(batches)} | {len(samples)} | {percentile(totals, .5):.3f} | {percentile(totals, .99):.3f} | {percentile(intervals, .5):.3f} | {percentile(intervals, .99):.3f} | {1000 * len(samples) / sum(batch["wall_ms"] for batch in batches):.2f} |')
        else:
            for batch in batches:
                run_journal = sum(row['journal_size'] for row in batch['suspended_invocations'] if row['target_handler_name'] == 'run' and 'LashTurn' in row['target_service_name'])
                label = str(rounds) if name == 'resume' else 'process'
                suspensions.append(f'| {label} | {run_journal} | {batch["suspended_journal_entries"]} | {batch["parked_ms"]:.3f} | {batch["external_completion_to_outcome_ms"]:.3f} | {totals[0]:.3f} |')
    return '\n'.join([
        'Tool round and complete-turn latency (milliseconds):', '',
        '| Tool rounds | Turns | Round p50 | Round p99 | Total p50 | Total p99 |',
        '|---:|---:|---:|---:|---:|---:|', *turns, '',
        'Write counts, averaged over completed turns:', '',
        '| Rounds | SQL statements/turn | Journal entries/turn | SQL/round | Journal/round | Combined/round |',
        '|---:|---:|---:|---:|---:|---:|', *writes, '',
        'Suspension and external completion (one sample per row; milliseconds):', '',
        '| Prior tool rounds | Turn run journal at suspension | All journal entries at suspension | Time parked after suspension | Completion request to outcome | Full send to outcome |',
        '|---|---:|---:|---:|---:|---:|', *suspensions, '',
        'Concurrent sessions, each with five rounds of three parallel tools:', '',
        '| Sessions | Pool maximum | Batches | Completed turns | Total p50 ms | Total p99 ms | Round p50 ms | Round p99 ms | Turns/s |',
        '|---:|---:|---:|---:|---:|---:|---:|---:|---:|', *concurrency, '',
    ])


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    source = parser.add_mutually_exclusive_group(required=True)
    source.add_argument('--inputs', nargs='+', type=Path)
    source.add_argument('--ledger', type=Path)
    parser.add_argument('--out', type=Path)
    args = parser.parse_args()
    if args.inputs:
        ledger = collect(args.inputs)
        if args.out:
            args.out.write_text(json.dumps(ledger, indent=2) + '\n')
    else:
        ledger = json.loads(args.ledger.read_text())
        if args.out:
            args.out.write_text(render(ledger))
    print(render(ledger))


if __name__ == '__main__':
    main()
