"""Strict matched-round kernel comparison; no retired VM or 3x verdict."""
import argparse
import json
from pathlib import Path
import statistics

COUNTS = ('allocations_run', 'allocations_start_and_run', 'reallocations_run',
          'reallocations_start_and_run', 'charged')
IDENTITY = ('workload_sha256', 'compiler', 'build_profile', 'debug_assertions',
            'allocator', 'split', 'fixed_iterations', 'cap', 'iterations', 'warmups', 'rounds')


def validate(row):
    rounds = row.get('rounds', 1)
    if row['split'] or not row['fixed_iterations'] or row['cap'] < 1 or rounds < 1:
        raise ValueError('comparison requires fixed, unsplit samples')
    if (row['iterations'] != row['cap'] or row['warmups'] != min(row['cap'], 20)
            or len(row['samples']) != row['iterations'] * rounds):
        raise ValueError('incomplete iteration population')
    for field in ('artifact_sha256', 'workload_sha256'):
        value = row[field]
        if len(value) != 64 or any(c not in '0123456789abcdef' for c in value):
            raise ValueError(f'missing {field}')
    for field in ('program', 'compiler', 'build_profile', 'allocator'):
        if not row[field].strip():
            raise ValueError(f'missing {field}')
    first = row['samples'][0]
    for index, sample in enumerate(row['samples']):
        if (sample['iteration'] != index % row['iterations']
                or sample.get('round', 1) != index // row['iterations'] + 1
                or sample['ns_run'] <= 0 or sample['ns_start_and_run'] < sample['ns_run']):
            raise ValueError('invalid iteration, round or time')
        if any(type(sample[key]) is not int or sample[key] < 0 for key in COUNTS):
            raise ValueError('allocation and charge records must be nonnegative integers')
        if any(sample[key] != first[key] for key in COUNTS):
            raise ValueError('allocation and charge counts changed within a workload')


def summarize(baseline, current):
    if not baseline['host'] or baseline['host'] != current['host']:
        raise ValueError('host identity mismatch: record a baseline on this runner')
    old = {row['program']: row for row in baseline['rows']}
    if len(old) != len(baseline['rows']) or not old or not current['rows']:
        raise ValueError('empty or duplicate comparison rows')
    seen = set()
    output = []
    for new in current['rows']:
        name = new['program']
        if name in seen or name not in old:
            raise ValueError(f'{name}: duplicate or absent baseline row')
        seen.add(name)
        previous = old[name]
        validate(previous)
        validate(new)
        for key in IDENTITY:
            if previous[key] != new[key]:
                raise ValueError(f'{name}: {key} identity mismatch')
        pairs = []
        for round_number in range(1, new['rounds'] + 1):
            def median(row):
                return statistics.median(sample['ns_run'] for sample in row['samples']
                    if sample['round'] == round_number)
            before, after = median(previous), median(new)
            pairs.append(dict(round=round_number, baseline_median_ns=before,
                              current_median_ns=after, ratio=after / before))
        counts = {key: dict(baseline=previous['samples'][0][key],
                           current=new['samples'][0][key],
                           delta=new['samples'][0][key] - previous['samples'][0][key])
                  for key in COUNTS}
        before_total = sum(sample['allocations_run'] for sample in previous['samples'])
        after_total = sum(sample['allocations_run'] for sample in new['samples'])
        counts['allocations_run_total'] = dict(baseline=before_total,
            current=after_total, delta=after_total - before_total)
        output.append(dict(program=name, baseline_artifact=previous['artifact_sha256'],
                           current_artifact=new['artifact_sha256'], pairs=pairs, counts=counts))
    return dict(advisory=True, baseline_recorded_at=baseline['recorded_at'], rows=output)


if __name__ == '__main__':
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('baseline', type=Path)
    parser.add_argument('current', type=Path)
    args = parser.parse_args()
    print(json.dumps(summarize(json.loads(args.baseline.read_text()),
                               json.loads(args.current.read_text())), indent=2))
