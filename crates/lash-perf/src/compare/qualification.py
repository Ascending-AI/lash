#!/usr/bin/env python3
"""Qualify one artifact against all references and the unchanged r53 pairs.

Build //crates/lash-perf:lash-perf__bin with --config=optimized, materialize
that artifact, then invoke this driver from the repository root through
`kiln gate lash <fork> -- bash -c 'ulimit -v 8388608 && exec python3 ...'`.
Supply --binary, a new --out directory, --expected-commit, the retained r53
--paired-contract, and --references-receipt from the pinned reference fetcher.
The original r53 per-leg receipts must remain beside its contract.json.

A second invocation refuses the same output directory. Infrastructure refusal
stops further measurements; unsupported rows remain unresolved. A completed
measurement with any target miss exits nonzero. An intrinsic-excess explanation
requires independent evidence and cannot be inferred or waived by this driver.
"""
import argparse
import hashlib
import json
import math
import pathlib
import re
import resource
import subprocess
import sys


ORIGINAL_PAIRS = (
    ('typescript', 'int_arith'), ('python', 'int_arith'),
    ('typescript', 'func_call'), ('python', 'func_call'),
    ('typescript', 'prop_read'), ('python', 'prop_read'),
    ('typescript', 'map_set_string'), ('python', 'map_set_string'),
    ('typescript', 'json_transform'), ('python', 'json_transform'),
    ('python', 'sort_bench'),
)


def require(condition, message):
    if not condition:
        raise ValueError(message)


def digest(path):
    return hashlib.sha256(pathlib.Path(path).read_bytes()).hexdigest()


def run(command, log):
    print(' '.join(map(str, command)), flush=True)
    with log.open('wb') as stream:
        result = subprocess.run(command, stdout=stream, stderr=subprocess.STDOUT)
    return result.returncode


def reference_table(receipt, inventory, target):
    rows = {(r['dialect'], r['workload'], r['engine']): r for r in receipt.get('rows', [])}
    lines = ['Lash uses scoped warm instruction secants; references use paired process secants. Exact charge and memory, runs=1, warmups=1, factor=20.', '',
             '| Dialect | Workload | Lash instructions/op | Reference instructions/op | Lash/reference | Status | Owners |',
             '|---|---|---:|---:|---:|---|---|']
    unresolved = 0
    target_misses = 0
    hard_misses = 0
    for name in inventory:
        for dialect, engine in [('typescript', 'qjs-0'), ('python', 'monty')]:
            candidate = rows.get((dialect, name, 'lash'))
            reference = rows.get((dialect, name, engine))
            number = lambda r: r.get('instructions', {}).get('median_per_operation') if r and r.get('instructions') else None
            lash, ref = number(candidate), number(reference)
            problem = any(f' {dialect} {name}:' in error for error in receipt.get('errors', []))
            equal_geometry = candidate and reference and all(candidate[key] == reference[key] for key in ['source_hash', 'trips', 'operations'])
            if dialect == 'python' and not rows.get((dialect, name, 'cpython')):
                problem = True
            valid = equal_geometry and not problem and lash is not None and ref is not None and math.isfinite(lash) and math.isfinite(ref) and lash > 0 and ref > 0
            ratio = lash / ref if valid else None
            if ratio is None:
                state = 'unresolved'
                unresolved += 1
            elif dialect == 'typescript':
                state = 'within 10x' if ratio <= 10 else 'above 10x'
            else:
                state = 'within target' if ratio <= 1 else 'above target, within 2x' if ratio <= 2 else 'above hard 2x'
            if name in ['int_arith', 'prop_read', 'empty_loop']:
                owners = 'A, C'
            elif name in ['func_call', 'func_closure_call', 'sort_bench', 'json_transform']:
                owners = 'B, C' if name != 'json_transform' else 'B, C, D'
            elif name in ['map_set_string', 'prop_write', 'records', 'object_build', 'object_sort', 'object_sort_200k']:
                owners = 'C, D'
            else:
                owners = 'C'
            if ratio is not None:
                target_misses += int(ratio > (1 if dialect == 'python' else 10))
                hard_misses += int(ratio > (2 if dialect == 'python' else 10))
            fmt = lambda n: 'unresolved' if n is None else f'{n:.2f}'
            lines.append(f'| {dialect} | {name} | {fmt(lash)} | {fmt(ref)} | {fmt(ratio)} | {state} | {owners} |')
    lines += ['', f'Unresolved reference rows: {unresolved}; target misses: {target_misses}; hard-ceiling misses: {hard_misses}. Unsupported rows and all raw counters remain in the receipt.']
    target.write_text('\n'.join(lines) + '\n')
    return unresolved, target_misses, hard_misses


def counter_refusal(errors):
    counter_failures = ('perf_event_open', 'pmu', 'unqualified instructions:u',
                        'missing instructions:u counter', 'unavailable instructions:u', 'multiplex')
    return any(any(word in error.lower() for word in counter_failures)
               or ('instructions:u' in error.lower()
                   and any(word in error.lower() for word in ('permission', 'not supported')))
               for error in errors)


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--binary', type=pathlib.Path, required=True)
    parser.add_argument('--out', type=pathlib.Path, required=True)
    parser.add_argument('--paired-contract', type=pathlib.Path, required=True)
    parser.add_argument('--references-receipt', type=pathlib.Path, required=True,
                        help='engines.json written by scripts/build-compare-references.py')
    parser.add_argument('--expected-commit', required=True)
    args = parser.parse_args()
    if resource.getrlimit(resource.RLIMIT_AS)[0] != 8388608 * 1024:
        parser.error('run under ulimit -v 8388608, preserving the 8 GiB cap')
    root = pathlib.Path.cwd()
    out = args.out.resolve()
    binary = args.binary.resolve()
    out.mkdir(parents=True, exist_ok=True)
    if (out / 'contract.json').exists():
        parser.error('refusing to overwrite an earlier measurement contract or retry a run')
    references = json.loads(args.references_receipt.read_text())
    qjs = pathlib.Path(references['quickjs']['qjs']).resolve()
    monty = pathlib.Path(references['monty']['binary']).resolve()
    require(references['monty']['version'] == 'Monty 1.4.0', 'reference must be Monty 1.4.0')
    require(references['monty']['binary_sha256'] == digest(monty), 'Monty binary hash changed')
    require(references['quickjs']['repository'] == 'https://github.com/bellard/quickjs.git', 'reference must be upstream QuickJS')
    require(references['quickjs']['revision'] == '04be246001599f5995fa2f2d8c91a0f198d3f34c', 'QuickJS revision changed')
    contract = json.loads(args.paired_contract.read_text())
    require(contract['factors'] == [2, 20] and contract['runs'] == contract['warmups'] == 1, 'r53 geometry changed')
    require(contract['memory_cap_kib'] == 8388608, 'r53 memory cap changed')
    require(tuple(map(tuple, contract['rows'])) == ORIGINAL_PAIRS, 'original r53 pair population changed')
    for dialect, workload in ORIGINAL_PAIRS:
        for factor in contract['factors']:
            require((args.paired_contract.parent / f'{dialect}-{workload}-f{factor}.json').is_file(), f'missing original r53 receipt: {dialect}/{workload}, factor {factor}')
    inventory = re.findall(r'name: "([^"]+)"', (root / 'crates/lash-perf/src/compare/workloads.rs').read_text())
    main_sha = subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip()
    if main_sha != args.expected_commit:
        parser.error(f'checkout {main_sha} does not match the pinned artifact commit {args.expected_commit}')
    manifest = {
        'sha': main_sha, 'binary': str(binary), 'binary_sha256': digest(binary),
        'references': {name: {'binary': str(path), 'sha256': digest(path)} for name, path in [
            ('quickjs', qjs), ('monty-1.4.0', monty), ('cpython', pathlib.Path('/usr/bin/python3'))]},
        'reference_versions': references,
        'paired_contract': contract, 'full_inventory_factor': 20,
        'memory_cap_kib': 8388608, 'runs': 1, 'warmups': 1,
        'method': 'Full inventory reference gate and original 11-row process-pair gate are separate observations; no retries or best-of selection.',
    }
    (out / 'contract.json').write_text(json.dumps(manifest, indent=2) + '\n')
    common = [str(binary), 'compare', '--runs', '1', '--warmups', '1', '--instructions']
    unresolved = 2 * len(inventory)
    target_misses = hard_misses = 0
    code = run(common + ['--factor', '20', '--out', str(out / 'references.json'),
               '--qjs', str(qjs), '--monty', str(monty), '--python', '/usr/bin/python3'], out / 'references.log')
    receipt_path = out / 'references.json'
    if receipt_path.exists():
        receipt = json.loads(receipt_path.read_text())
        unresolved, target_misses, hard_misses = reference_table(receipt, inventory, out / 'reference-table.md')
        errors = receipt.get('errors', [])
        if counter_refusal(errors):
            print('Infrastructure refusal. No retry and no further measurements.', flush=True)
            return code or 1
    elif code:
        print('Comparison failed before a receipt. No retry.', flush=True)
        return code
    if code < 0 or code in [137, 139]:
        print('Comparison process terminated. No retry or further measurements.', flush=True)
        return code
    paired = out / 'paired'
    paired.mkdir(exist_ok=True)
    rows = []
    for dialect, workload in contract['rows']:
        receipts = []
        counts = []
        for factor in contract['factors']:
            stem = paired / f'{dialect}-{workload}-f{factor}'
            command = ['perf', 'stat', '--no-big-num', '--no-scale', '-x', '\t', '-e', 'instructions:u', '-o', str(stem.with_suffix('.stat')), '--']
            command += common + ['--factor', str(factor), '--out', str(stem.with_suffix('.json')), '--dialect', dialect, '--workload', workload]
            status = run(command, stem.with_suffix('.log'))
            if status:
                print(f'Pair incomplete: {dialect}/{workload}, factor {factor}, exit {status}. No retry.', flush=True)
                return status
            receipt = json.loads(stem.with_suffix('.json').read_text())
            prior_path = args.paired_contract.parent / stem.with_suffix('.json').name
            prior = json.loads(prior_path.read_text())['rows'][0]
            current = receipt['rows'][0]
            for key in ['dialect', 'workload', 'source_hash', 'trips', 'operations']:
                require(current[key] == prior[key], f'changed r53 {key}: {stem}')
            require([[s['result'] for s in pair] for pair in current['samples']] == [[s['result'] for s in pair] for pair in prior['samples']], f'changed r53 checksum: {stem}')
            receipts.append(receipt)
            line = next(line for line in stem.with_suffix('.stat').read_text().splitlines() if '\tinstructions:u\t' in line)
            fields = line.split('\t')
            if not 99 <= float(fields[4]) <= 100:
                print('Unqualified process counter. No retry.', flush=True)
                return 1
            counts.append(int(fields[0]))
        low = receipts[0]['rows'][0]
        high = receipts[1]['rows'][0]
        require(low['source_hash'] == high['source_hash'], 'pair source hash differs')
        require(low['engine_identity'] == high['engine_identity'] == manifest['binary_sha256'], 'pair artifact hash differs')
        require(low['samples'][0][0]['result'] == high['samples'][0][0]['result'], 'pair fixed-low checksum differs')
        denominator = 2 * (high['operations'][1] - low['operations'][1])
        rows.append((dialect, workload, high['instructions']['median_per_operation'], (counts[1] - counts[0]) / denominator, counts, denominator))
        (paired / 'measurements.json').write_text(json.dumps(rows, indent=2) + '\n')
    lines = ['| Dialect | Workload | Scoped warm instructions/op | Process-pair instructions/op | Raw pair | Denominator |', '|---|---|---:|---:|---|---:|']
    lines += [f'| {d} | {w} | {scoped:.2f} | {paired:.2f} | {counts} | {denominator} |' for d, w, scoped, paired, counts, denominator in rows]
    (out / 'paired.md').write_text('\n'.join(lines) + '\n')
    print(f'Complete: {len(rows)} original process pairs; reference exit {code}.', flush=True)
    print(f'Reference verdict: {unresolved} unresolved; {target_misses} target misses; {hard_misses} hard-ceiling misses. No intrinsic-excess waiver is inferred.', flush=True)
    return code or int(bool(unresolved or target_misses))


if __name__ == '__main__':
    sys.exit(main())
