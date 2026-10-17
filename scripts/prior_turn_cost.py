#!/usr/bin/env python3
"""Read matched replay heap captures as separate turn and compaction series."""
from __future__ import annotations

import argparse
import hashlib
import json
import math
import re
import statistics
from pathlib import Path


def digest(value: object) -> str:
    return hashlib.sha256(json.dumps(value, sort_keys=True, separators=(',', ':')).encode()).hexdigest()


def experiment_identity(configuration: dict, observations: list[dict], window: dict) -> dict:
    payload = dict(configuration=configuration,
                   runs=[dict(mode=r['mode'], identity=r['identity']) for r in observations],
                   heap_window=window)
    return dict(id=digest(payload), **payload)


def slopes(points: list[tuple[int, float]]) -> dict:
    """Unknown slopes stay absent; uncertainty is descriptive, never a gate."""
    n = len(points)
    depths = len({x for x, _ in points})
    result = dict(n=n, distinct_depths=depths, least_squares=None, theil_sen=None,
                  least_squares_standard_error=None, pairwise_slope_range=None)
    if depths < 2:
        return result
    mx = statistics.mean(x for x, _ in points)
    my = statistics.mean(y for _, y in points)
    xx = sum((x - mx)**2 for x, _ in points)
    slope = sum((x - mx)*(y - my) for x, y in points) / xx
    pairs = [(y2-y1)/(x2-x1) for i, (x1, y1) in enumerate(points)
             for x2, y2 in points[i+1:] if x1 != x2]
    result.update(least_squares=slope, theil_sen=statistics.median(pairs),
                  pairwise_slope_range=[min(pairs), max(pairs)])
    if n > 2:
        result['least_squares_standard_error'] = math.sqrt(
            sum((y-my-slope*(x-mx))**2 for x, y in points) / (n-2) / xx)
    return result


def metrics(rows: list[dict], units: dict) -> dict:
    return {name: dict(unit=unit, aggregation='round total / sessions',
                       history=slopes([(r['history_depth'], r['metrics'][name]/r['sessions'])
                                       for r in rows if name in r['metrics']]),
                       frame=slopes([(r['frame_depth'], r['metrics'][name]/r['sessions'])
                                     for r in rows if name in r['metrics']]))
            for name, unit in units.items() if any(name in r['metrics'] for r in rows)}


def read_series(captures: list[Path]) -> tuple[dict, dict]:
    identities, experiments, samples, compactions, site_samples, overhead = [], [], [], [], [], []
    seen = set()
    for capture in captures:
        receipt = json.loads((capture / 'receipt.json').read_text())
        experiment = receipt['experiment']
        if (receipt['kind'] != 'lash.replay-heap' or experiment != experiment_identity(
                experiment['configuration'], receipt['observations'], receipt['window_receipt'])):
            raise ValueError('invalid experiment identity')
        if experiment['id'] in seen:
            raise ValueError('duplicate experiment; one capture cannot inflate the population')
        seen.add(experiment['id'])
        experiments.append(experiment)
        configuration = experiment['configuration']
        for name in ('observer', 'backend', 'worker', 'window', 'terminal', 'useful_work', 'executables'):
            if not configuration.get(name):
                raise ValueError(f'missing experiment {name}')
        if configuration['terminal'] != 'all scripted replies consumed; all turns answered; clean shutdown':
            raise ValueError('incomplete terminal identity')
        observations = receipt['observations']
        if [r['mode'] for r in observations] != ['off', 'dhat'] or any(r['exit_code'] for r in observations):
            raise ValueError('incomplete observer pair')
        comparable = dict(configuration)
        comparable.pop('window')
        identities.append(comparable)
        rounds_by_mode = []
        for observation in observations:
            replay = json.loads((capture / observation['mode'] / 'replay.json').read_text())
            if replay['identity'] != observation['identity'] or len(replay['workloads']) != 1:
                raise ValueError('replay identity differs from producer receipt')
            stable_identity = {key: value for key, value in replay['identity'].items()
                               if key not in ('started_at', 'load1_at_start')}
            # Heap geometry is the deliberate off/on difference, not a
            # workload identity. Validate it before comparing the rest.
            expected_window = None if observation['mode'] == 'off' else configuration['window']
            if stable_identity.get('heap_window') != expected_window:
                raise ValueError('on/off heap window differs from selected window')
            stable_identity['heap_window'] = None
            if stable_identity != configuration.get('producer'):
                raise ValueError('on/off producer configurations differ')
            workload = replay['workloads'][0]
            if (workload['backend'] != configuration['backend'] or
                    workload['script_digest'] != configuration['script_digest'] or
                    len(workload['rounds']) != receipt['turns']):
                raise ValueError('workload identity or terminal population differs')
            selected = [r for r in workload['rounds'] if r['turn'] == receipt['profile_turn']]
            if len(selected) != 1 or selected[0].get('setup') or selected[0]['sessions'] != 1:
                raise ValueError('select exactly one non-setup session turn')
            rounds_by_mode.append(selected[0])
            if observation['mode'] == 'off':
                units = workload['units']
                compactions.append((receipt['turns'], workload['compactions']))
        window = receipt['window_receipt']
        if (not window['completed'] or window['role'] != 'replay-host' or not window['process_id']
                or window['first_turn'] != receipt['profile_turn']
                or window['last_turn'] != receipt['profile_turn']
                or receipt['profile_through_turn'] != receipt['profile_turn']
                or configuration['window'] != [receipt['profile_turn'], receipt['profile_turn']]):
            raise ValueError('incomplete or mismatched selected-turn window')
        for key in ('history_depth', 'frame_depth', 'sessions', 'turn'):
            if rounds_by_mode[0][key] != rounds_by_mode[1][key]:
                raise ValueError('on/off useful-work populations differ')
        samples.append(dict(experiment=experiment, capture=str(capture), **rounds_by_mode[0]))
        profile = json.loads((capture / 'dhat/parent.dhat.json').read_text())
        sites = {}
        for pp in profile['pps']:
            # Keep the full source stack, not addresses or one arbitrarily selected frame.
            stack = tuple(re.sub(r'0x[0-9a-fA-F]+', '<address>', profile['ftbl'][index])
                          for index in pp['fs'])
            site = sites.setdefault(stack, dict(allocated_traffic_bytes=0, window_end_live_bytes=0,
                                                simultaneous_peak_live_bytes=0))
            for name, key in [('allocated_traffic_bytes', 'tb'), ('window_end_live_bytes', 'eb'),
                              ('simultaneous_peak_live_bytes', 'gb')]:
                site[name] += pp[key]
        site_samples.append((rounds_by_mode[0]['history_depth'], sites))
        overhead.append(dict(experiment=experiment, observations=observations,
                             instructions_on_minus_off=(observations[1]['instructions_u'] - observations[0]['instructions_u']
                                 if all(o.get('instructions_u') is not None for o in observations) else None),
                             scope='whole process including prior turns, startup, shutdown and DHAT flush',
                             diagnostic=True))
    if not identities or any(identity != identities[0] for identity in identities):
        raise ValueError('mixed experiment configurations; refuse a qualified comparison')
    site_rows = []
    for stack in sorted(set().union(*(sites for _, sites in site_samples))):
        values = [dict(history_depth=depth, **sites.get(stack, dict(
            allocated_traffic_bytes=0, window_end_live_bytes=0, simultaneous_peak_live_bytes=0)))
                  for depth, sites in site_samples]
        site_rows.append(dict(stack=list(stack), samples=values, quantities={
            key: slopes([(r['history_depth'], r[key]) for r in values])
            for key in ('allocated_traffic_bytes', 'window_end_live_bytes', 'simultaneous_peak_live_bytes')}))
    frame_table = sorted({frame for site in site_rows for frame in site['stack']})
    frame_ids = {frame: index for index, frame in enumerate(frame_table)}
    for site in site_rows:
        site['stack'] = [frame_ids[frame] for frame in site['stack']]
    common = dict(certifying=False, population='one session per round',
                  experiments=experiments,
                  uncertainty='OLS residual standard error (n>2); pairwise slope min/max is descriptive, not a confidence interval',
                  call_ledger_join=None)
    prior = dict(common, kind='lash.prior-turn-cost', samples=samples, metrics=metrics(samples, units),
                 source_sites=site_rows, frame_table=frame_table, overhead=overhead,
                 heap_scope='host Rust allocations born in selected turn; absent site in complete DHAT capture means zero traffic',
                 retained_history_live_bytes=None, worker_heap_bytes=None, native_heap_bytes=None)
    # Select one complete history from the deepest off run. Shorter fresh histories
    # repeat its early depths; pooling them would weight those depths more heavily.
    # Ordinary turns never enter this population.
    deepest = max(range(len(compactions)), key=lambda i: compactions[i][0])
    raw_compactions = compactions[deepest][1]
    compact = dict(common, kind='lash.compaction-cost', samples=raw_compactions,
                   source_capture=str(captures[deepest]), source_experiment=common['experiments'][deepest],
                   metrics=metrics(raw_compactions, units),
                   selection='all raw compactions from deepest inactive-profiler capture, no prefix duplicates')
    return prior, compact


def write_series(captures: list[Path], out: Path) -> None:
    prior, compact = read_series(captures)
    out.mkdir(parents=True, exist_ok=True)
    for name, report in [('prior-turn-cost', prior), ('compaction-cost', compact)]:
        (out / f'{name}.json').write_text(json.dumps(report, separators=(',', ':'), allow_nan=False)+'\n')
        depths = len({r['history_depth'] for r in report['samples']})
        print(f'{name}: n={len(report["samples"])} distinct_depths={depths}; diagnostic, no budget')
        for metric, value in report['metrics'].items():
            s = value['history']
            print(f'  {metric} [{value["unit"]}/prior-turn]: n={s["n"]} depths={s["distinct_depths"]} '
                  f'OLS={s["least_squares"]} SE={s["least_squares_standard_error"]} '
                  f'Theil-Sen={s["theil_sen"]} pair-range={s["pairwise_slope_range"]}')
    print(f'Source-site stacks: {len(prior["source_sites"])}; tb=traffic, eb=window-end live, gb=simultaneous-peak live')
    ranked = sorted(prior['source_sites'], key=lambda site: abs(
        site['quantities']['allocated_traffic_bytes']['theil_sen'] or 0), reverse=True)
    for site in ranked[:10]:
        slope = site['quantities']['allocated_traffic_bytes']
        stack = [prior['frame_table'][index] for index in site['stack']]
        frames = [frame for frame in stack if 'lash' in frame]
        frames = [frame if len(frame) <= 400 else frame[:180]+' ... '+frame[-180:]
                  for frame in (frames or stack)[:3]]
        print(f'  site traffic bytes/prior-turn: n={slope["n"]} depths={slope["distinct_depths"]} '
              f'OLS={slope["least_squares"]} SE={slope["least_squares_standard_error"]} '
              f'Theil-Sen={slope["theil_sen"]} pair-range={slope["pairwise_slope_range"]}: '
              + ' | '.join(frames))
    print('Preexisting retained history, worker/native heaps and call-ledger join: unavailable')
    for row in prior['overhead']:
        print(f'Observer pair instructions:u on-minus-off={row["instructions_on_minus_off"]}; whole-process diagnostic')


def main() -> None:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('captures', nargs='+', type=Path)
    parser.add_argument('--out-dir', required=True, type=Path)
    args = parser.parse_args()
    write_series(args.captures, args.out_dir)


if __name__ == '__main__':
    main()
