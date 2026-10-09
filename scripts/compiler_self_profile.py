#!/usr/bin/env python3
"""Build selected rustc self-profiles through Kiln and print phase summaries."""
import argparse
import json
from pathlib import Path
import shutil
import subprocess
import sys
import time

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from tools.buck2.outputs import resolve


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('labels', nargs='+', help='explicit first-party library or binary labels')
    parser.add_argument('--out-dir', type=Path, required=True, help='new directory for build receipts and summaries')
    parser.add_argument('--summarize', default='summarize', help='measureme summarize executable')
    parser.add_argument('--top', type=int, default=15, help='number of queries/phases to print')
    parser.add_argument('--cold', action='store_true', help='use a fresh isolation directory and bypass remote cache')
    args = parser.parse_args()
    if args.top <= 0:
        parser.error('--top must be positive')
    decoder = shutil.which(args.summarize)
    if decoder is None:
        parser.error('measureme summarize is missing; pass --summarize=/path/to/summarize')
    root = Path(__file__).resolve().parents[1]
    out = args.out_dir.resolve()
    out.mkdir(parents=True, exist_ok=False)
    report = out / 'build.json'
    command = ['kiln', 'build', '--config=rust-self-profile', '--materializations=final',
               '--build-report', str(report), *args.labels]
    if args.cold:
        command += ['--isolation-dir', 'self-profile-' + str(time.time_ns()), '--no-remote-cache']
    print('Building: ' + ' '.join(command), flush=True)
    started = time.monotonic()
    subprocess.run(command, cwd=root, check=True)
    elapsed = time.monotonic() - started
    (out / 'invocation.json').write_text(json.dumps({
        'command': command, 'build_wall_seconds': elapsed, 'decoder': decoder,
    }, indent=2) + '\n')
    payload = json.loads(report.read_text())
    for index, label in enumerate(args.labels):
        paths = resolve(payload, label, 'profile|rustc_stages|raw')
        if len(paths) != 1 or not paths[0].endswith('.mm_profdata'):
            raise ValueError(f'expected one .mm_profdata output for {label}, found {paths}')
        summary = subprocess.run([decoder, 'summarize', paths[0]], check=True,
                                 capture_output=True, text=True).stdout
        (out / f'{index}-summary.txt').write_text(summary)
        print(f'\n{label}: top {args.top} queries/phases by self time\n{paths[0]}')
        rows = 0
        for line in summary.splitlines():
            # measureme prints a table header, then descending self-time rows.
            if line.startswith('|') and rows > 0:
                if rows > args.top:
                    continue
                rows += 1
            elif line.startswith('|'):
                rows = 1
            print(line)
            if rows > args.top:
                print('(remaining rows are in the saved summary)')
                break
        for line in summary.splitlines():
            if line.startswith('Total cpu time:'):
                print(line)
    print(f'\nBuild wall time: {elapsed:.3f}s (shared host; diagnostic observation)')


if __name__ == '__main__':
    try:
        main()
    except (OSError, ValueError, subprocess.CalledProcessError) as error:
        raise SystemExit(f'compiler-self-profile: {error}') from error
