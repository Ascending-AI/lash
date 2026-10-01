#!/usr/bin/env python3
"""Run two concurrent filtered `kiln test` repeat loops on one binary and count cross-talk.

    concurrent_selection_repro.py OUT [--runs N] [--target LABEL] [--local] FIRST SECOND

FIRST and SECOND are exact libtest names in LABEL. Each invocation repeats its
own name N times into OUT/<a|b>. A repetition whose JUnit report names any
case but its own is cross-talk. This is a live reproduction for a fork with
the remote pool, not a unit test: test_external_runner.py simulates the same
race.
"""
import argparse
import json
from pathlib import Path
import subprocess
import sys
import xml.etree.ElementTree as ET


def observed(directory):
    """Map each repetition to the case names its reported JUnit file holds."""
    names = {}
    for report in sorted(directory.glob('run-*/test-report.json'), key=lambda path: int(path.parent.name[4:])):
        cases = []
        for result in json.loads(report.read_text()).get('results', {}).values():
            xml = (result.get('outputs') or {}).get('junit_xml')
            if xml:
                cases += [case.get('name') for case in ET.parse(xml).getroot().iter('testcase')]
        names[report.parent.name] = cases
    return names


def main():
    parser = argparse.ArgumentParser(description=__doc__, formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument('out', type=Path)
    parser.add_argument('first')
    parser.add_argument('second')
    parser.add_argument('--runs', type=int, default=25)
    parser.add_argument('--target', default='//crates/lash-core-ids:lash-core-ids__unit_test')
    parser.add_argument('--local', action='store_true', help='execute the tests on this host, as service tests do')
    options = parser.parse_args()
    out = options.out.absolute()
    out.mkdir(parents=True, exist_ok=True)
    processes = []
    for side, name in (('a', options.first), ('b', options.second)):
        argv = ['kiln', 'test', options.target, f'--runs_per_test={options.runs}', '--test_arg=--exact', f'--test_arg={name}', '--test-output-dir', str(out / side)]
        if options.local:
            argv.append('--local-test-execution')
        log = (out / f'{side}.log').open('w')
        processes.append((side, name, subprocess.Popen(argv, stdout=log, stderr=subprocess.STDOUT)))
    crossed = total = 0
    for side, name, process in processes:
        code = process.wait()
        runs = observed(out / side)
        wrong = {run: cases for run, cases in runs.items() if cases != [name]}
        crossed += len(wrong)
        total += len(runs)
        print(f'{side}: exit {code}, {len(runs)} reported runs, {len(wrong)} not naming exactly {name}')
        for run, cases in wrong.items():
            print(f'  {run}: {cases}')
    print(f'cross-talk: {crossed}/{total} runs')
    return 1 if crossed else 0


if __name__ == '__main__':
    sys.exit(main())
