#!/usr/bin/env python3
"""Count workflow cells with real host awaits through the kernel instrument."""
import argparse
import hashlib
import json
from pathlib import Path
import statistics
import subprocess

import embedded


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, help='existing optimized kernel_perf artifact')
    parser.add_argument('--out', type=Path, required=True)
    args = parser.parse_args()
    out = args.out.resolve()
    out.mkdir(parents=True, exist_ok=False)
    binary = args.kernel.resolve() if args.kernel else Path(embedded.build(out))
    programs = Path(__file__).with_name('workflows')
    expected = {
        'workflow_iteration': [f'item-{n}' for n in range(128)],
        'workflow_early_return': ['negative'] * 64 + ['done'] * 64,
    }
    receipt = dict(
        revision=subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=embedded.ROOT, text=True).strip(),
        artifact_sha256=hashlib.sha256(binary.read_bytes()).hexdigest(),
        window='cold process: preparation, one workflow, finish, receipt, teardown',
        samples=[],
    )
    for name, result in expected.items():
        for pair, dialects in enumerate([('ts', 'py'), ('py', 'ts')], 1):
            for dialect in dialects:
                source = programs / f'{name}.{dialect}'
                sample, output = embedded.measure(
                    [str(binary), 'embedded', str(source)], out / f'{name}-{pair}-{dialect}', 1800)
                value, inner = embedded.parse_result('lash-' + dialect, output)
                digest = embedded.digest(value)
                if inner['outcome'] != 'ok' or digest != embedded.digest(result):
                    raise ValueError(f'{name}/{dialect}: unsuccessful or incorrect workflow: {inner}')
                sample.update(program=name, dialect=dialect, pair=pair, result_sha256=digest,
                              source_sha256=hashlib.sha256(source.read_bytes()).hexdigest(), lash=inner)
                receipt['samples'].append(sample)
                print(f'{name}/{dialect} pair={pair}: {sample["instructions"]} instr/op; checksum={digest}', flush=True)
    (out / 'receipt.json').write_text(json.dumps(receipt, indent=2) + '\n')
    for name in expected:
        for dialect in ('ts', 'py'):
            rows = [row for row in receipt['samples'] if (row['program'], row['dialect']) == (name, dialect)]
            print(f'{name}/{dialect}: mean {statistics.mean(row["instructions"] for row in rows):.0f} instr/op')


if __name__ == '__main__':
    main()
