"""Gate 6: retain kernel process pairs, compare recorded kernel rows, feed history.

Wall times and allocation totals are diagnostics. Completeness and identity
refusals fail the caller; a noisy runner never fails a wall-time threshold.
"""
import argparse
import csv
import datetime
import hashlib
import json
import os
from pathlib import Path
import resource
import subprocess
import sys

from summarize import summarize, validate

csv.field_size_limit(8 * 1024 * 1024)


def sha256(path):
    return hashlib.sha256(Path(path).read_bytes()).hexdigest()


def host_identity():
    model = next((line for line in Path('/proc/cpuinfo').read_text().splitlines()
                  if line.startswith('model name')), '')
    facts = (Path('/etc/machine-id').read_text().strip(), os.uname().sysname,
             os.uname().machine, model, os.uname().release)
    return hashlib.sha256(';'.join(facts).encode()).hexdigest()


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--kernel', type=Path, required=True)
    parser.add_argument('--reader', type=Path)
    parser.add_argument('--history', type=Path)
    parser.add_argument('--out', type=Path, required=True)
    parser.add_argument('--baseline', type=Path)
    parser.add_argument('--record-baseline', action='store_true')
    parser.add_argument('--iterations', type=int, default=3)
    parser.add_argument('--rounds', type=int, default=3)
    parser.add_argument('programs', nargs='*')
    args = parser.parse_args()
    if bool(args.reader) != bool(args.history):
        parser.error('--reader and --history must be supplied together')
    if args.iterations < 1 or args.rounds < 1:
        parser.error('iterations and rounds must be positive')
    if bool(args.baseline) == args.record_baseline:
        parser.error('select a recorded --baseline or --record-baseline')
    if os.environ.get('KERNEL_PERF_OBSERVE', ''):
        parser.error('ordinary kernel comparison requires KERNEL_PERF_OBSERVE unset or empty')
    # The same cap applies to each instrument child, including history reading.
    cap = 8 * 1024**3
    soft, hard = resource.getrlimit(resource.RLIMIT_AS)
    ceiling = min(cap, hard) if hard != resource.RLIM_INFINITY else cap
    resource.setrlimit(resource.RLIMIT_AS, (ceiling, ceiling))
    args.out.mkdir(parents=True, exist_ok=True)
    environment = dict(os.environ, KERNEL_PERF_SPLIT='0', KERNEL_PERF_FIXED='1')
    artifact = sha256(args.kernel)
    available = subprocess.check_output([str(args.kernel), '--list-programs'], text=True).splitlines()
    programs = args.programs or available
    if not programs or len(set(programs)) != len(programs):
        raise ValueError('selection must be nonempty and unique')
    baseline = json.loads(args.baseline.read_text()) if args.baseline else None
    grouped = {program: [] for program in programs}
    for round_number in range(1, args.rounds + 1):
        for index, program in enumerate(programs):
            identity = f'{round_number}-{index}'
            command = [str(args.kernel), str(args.iterations), program]
            # GNU time writes resources separately, keeping stdout a strict TSV.
            before = os.getloadavg()
            result = subprocess.run(['/usr/bin/time', '-v', '-o', str(args.out / f'{identity}.resources'),
                                     *command], env=environment, text=True, capture_output=True,
                                    check=False)
            (args.out / f'{identity}.stdout').write_text(result.stdout)
            (args.out / f'{identity}.stderr').write_text(result.stderr)
            with (args.out / 'host-load.jsonl').open('a') as output:
                output.write(json.dumps(dict(round=round_number, program=program,
                    before=before, after=os.getloadavg(), returncode=result.returncode,
                    command=command, artifact_sha256=artifact)) + '\n')
            result.check_returncode()
            rows = list(csv.DictReader(result.stdout.splitlines(), delimiter='\t'))
            if len(rows) != 1 or rows[0]['program'] != program or None in rows[0]:
                raise ValueError(f'{program}: incomplete TSV selection')
            row = json.loads(rows[0]['receipt'])
            validate(row)
            if (row['program'] != program or row['artifact_sha256'] != artifact or row['cap'] != args.iterations
                    or row['iterations'] != args.iterations):
                raise ValueError(f'{program}: artifact or geometry mismatch')
            if int(rows[0]['charged']) != row['samples'][0]['charged']:
                raise ValueError(f'{program}: TSV charge differs from raw charge')
            for sample in row['samples']:
                sample['round'] = round_number
            grouped[program].append(row)
    receipt = dict(host=host_identity(), commit=subprocess.check_output(
        ['git', 'rev-parse', 'HEAD'], text=True).strip(),
        run_id=os.environ.get('GITHUB_RUN_ID', args.out.name),
        recorded_at=datetime.datetime.now(datetime.timezone.utc).isoformat(), rows=[])
    for program, rounds in grouped.items():
        row = dict(rounds[0])
        row['samples'] = [sample for current in rounds for sample in current['samples']]
        row['rounds'] = args.rounds
        for current in rounds[1:]:
            for key in row.keys() - {'samples', 'rounds'}:
                if current[key] != row[key]:
                    raise ValueError(f'{program}: {key} changed between processes')
        validate(row)
        receipt['rows'].append(row)
    receipt_path = args.out / 'receipt.json'
    receipt_path.write_text(json.dumps(receipt, indent=2) + '\n')
    if baseline is not None:
        summary = summarize(baseline, receipt)
        (args.out / 'comparison.json').write_text(json.dumps(summary, indent=2) + '\n')
        print(json.dumps(summary, indent=2))
    if args.reader:
        result = subprocess.run(['/usr/bin/time', '-v', '-o', str(args.out / 'history.resources'),
            str(args.reader), 'kernel-history', '--receipt', str(receipt_path),
            '--history', str(args.history)], text=True, capture_output=True, check=False)
        (args.out / 'history.txt').write_text(result.stdout)
        (args.out / 'history.stderr').write_text(result.stderr)
        print(result.stdout, end='')
        print(result.stderr, end='', file=sys.stderr)
        result.check_returncode()
        # The maintained consumer reaches both the Actions log and summary.
        if os.environ.get('GITHUB_STEP_SUMMARY'):
            with Path(os.environ['GITHUB_STEP_SUMMARY']).open('a') as output:
                output.write('### Kernel gate 6 (advisory)\n\n```text\n')
                output.write(result.stdout + result.stderr + '\n```\n')


if __name__ == '__main__':
    main()
