#!/usr/bin/env python3
"""Run the on-demand Confidence stage plan on this host."""
from __future__ import annotations

import argparse
import asyncio
from dataclasses import dataclass
import fnmatch
import hashlib
import itertools
import json
import os
from pathlib import Path
import re
import shlex
import signal
import subprocess
import sys
import time
import shutil
import tomllib


ROOT = Path(__file__).resolve().parents[2]


@dataclass(frozen=True)
class Stage:
    name: str
    group: str
    command: tuple[str, ...]
    environment: dict[str, str]
    timeout: int
    needs_build: bool

    @property
    def heavy(self) -> bool:
        return self.name.startswith('mutation-') or self.name == 'coverage'


def stages(root: Path, run_index: int, salt: str) -> list[Stage]:
    plan = json.loads((root / 'scripts/confidence-stages.json').read_text())
    context = {'root': str(root), 'run_index': run_index, 'salt': salt}
    result = []
    for stage in plan['stages']:
        matrix = stage.get('matrix', {})
        axes = {key: value for key, value in matrix.items() if key != 'include'}
        rows = [dict(zip(axes, values)) for values in itertools.product(*axes.values())] if axes else []
        rows.extend(matrix.get('include', []))
        for row in rows or [{}]:
            values = context | row
            name = stage['name'].format_map(values)
            if not re.fullmatch(r'[a-zA-Z0-9_-]+', name):
                raise ValueError(f'Unsafe stage name: {name}')
            environment = {key: value.format_map(values)
                           for key, value in (plan['environment'] | stage['environment']).items()}
            command = tuple(arg.format_map(values) for arg in stage['command'])
            result.append(Stage(name, stage['group'], command, environment,
                                stage['timeout_seconds'], stage['needs_build']))
    if len({stage.name for stage in result}) != len(result):
        raise ValueError('Duplicate local stage names')
    return result


def default_jobs() -> int:
    cpus = len(os.sched_getaffinity(0)) if hasattr(os, 'sched_getaffinity') else os.cpu_count() or 1
    memory = next((int(line.split()[1]) * 1024 for line in Path('/proc/meminfo').read_text().splitlines()
                   if line.startswith('MemAvailable:')), 12 * 1024**3)
    # A stage may launch four mutant builds itself. Reserve 12 GiB per slot
    # and four CPUs, with at most eight stage processes on a large host.
    return max(1, min(8, cpus // 4, memory // (12 * 1024**3)))


def source_signature(root: Path) -> str:
    paths = ['crates', 'examples', 'runbooks', 'Cargo.toml', 'Cargo.lock',
             'scripts/confidence-stages.json', 'scripts/confidence-gate.sh',
             'scripts/ci/confidence-stage.sh', 'scripts/ci/confidence_local.py',
             'scripts/confidence-local.sh']
    listed = subprocess.check_output(
        ['git', 'ls-files', '--cached', '--others', '--exclude-standard', '-z', '--', *paths],
        cwd=root).decode().split('\0')
    names = set(filter(None, listed)) | {name for name in paths if (root / name).is_file()}
    digest = hashlib.sha256()
    for name in sorted(names):
        path = root / name
        digest.update(name.encode() + b'\0')
        digest.update(path.read_bytes() if path.is_file() else b'deleted')
    config = root / '.cargo/mutants.toml'
    if config.exists():
        selection = tomllib.loads(config.read_text())
        for key in ('gitignore', 'copy_target', 'copy_vcs'):
            selection.pop(key, None)
        digest.update(json.dumps(selection, sort_keys=True).encode())
    return digest.hexdigest()


async def terminate(process: asyncio.subprocess.Process) -> None:
    if process.returncode is not None:
        return
    os.killpg(process.pid, signal.SIGTERM)
    try:
        await asyncio.wait_for(process.wait(), 10)
    except asyncio.TimeoutError:
        os.killpg(process.pid, signal.SIGKILL)
        await process.wait()


async def run_stage(stage: Stage, output: Path, root: Path) -> dict:
    destination = output / stage.name
    if destination.exists():
        archive = output / 'attempts' / f'{stage.name}-{time.time_ns()}'
        archive.parent.mkdir(exist_ok=True)
        shutil.move(str(destination), archive)
        previous_log = output / 'logs' / f'{stage.name}.log'
        if previous_log.exists():
            shutil.move(str(previous_log), archive / 'process.log')
    destination.mkdir()
    scratch = destination / 'tmp'
    scratch.mkdir()
    environment = dict(os.environ)
    # Coordinates belong to the local plan, never to a previously selected shard.
    for key in ('LASH_CONFIDENCE_STAGE', 'LASH_SIM_SHARD', 'LASH_MUTATION_SIM_GROUP',
                'LASH_MUTATION_SIM_SHARD', 'LASH_CONFIDENCE_PACKAGE', 'LASH_MUTATION_PACKAGES_SHARD',
                'LASH_POSTGRES_DATABASE_URL'):
        environment.pop(key, None)
    environment.update(stage.environment)
    environment.update(LASH_CONFIDENCE_OUT_DIR=str(destination), TMPDIR=str(scratch))
    # The existing mutation service discovers the assigned port through Docker
    # inspect. Publishing port zero gives every mutation shard its own server.
    environment['LASH_CONFIDENCE_MUTATION_POSTGRES_PORT'] = '0'
    environment['LASH_MUTATION_RUN_INDEX'] = str(output_run_index(output))
    target = Path(environment.get('CARGO_TARGET_DIR', str(root / 'target')))
    environment['LASH_VM_WORKER'] = str(target / 'debug/lash-vm-worker')
    if stage.name == 'build':
        # This named Cargo recipe produces the tree consumed by local Cargo
        # stages. Retain Cargo artifacts and its managed admission, rather
        # than routing --no-run to a Buck2 test execution.
        environment['KILN_CARGO_ROUTE'] = ''
    command = stage.command
    if stage.name in {'backends', 'coverage'}:
        environment.pop('LASH_POSTGRES_DATABASE_URL', None)
        command = ('bash', 'scripts/ci/with-service.sh', 'pg', '--', *command)
    started = time.monotonic()
    process = None
    status = 'failure'
    code = None
    logs = output / 'logs'
    logs.mkdir(exist_ok=True)
    log_path = logs / f'{stage.name}.log'
    with log_path.open('w') as log:
        try:
            process = await asyncio.create_subprocess_exec(
                *command, cwd=root, env=environment, stdout=log,
                stderr=asyncio.subprocess.STDOUT, start_new_session=True)
            code = await asyncio.wait_for(process.wait(), stage.timeout)
            status = 'success' if code == 0 else 'failure'
        except asyncio.TimeoutError:
            log.write(f'\nStage exceeded stage timeout ({stage.timeout}s).\n')
            await terminate(process)
            status = 'cancelled'
        except asyncio.CancelledError:
            if process:
                await terminate(process)
            raise
        except OSError as error:
            log.write(f'Unable to start stage: {error}\n')
    record = {'stage': stage.name, 'group': stage.group, 'result': status,
              'exit_code': code, 'seconds': round(time.monotonic() - started, 3),
              'command': list(command), 'environment': stage.environment,
              'output': str(destination), 'log': str(log_path)}
    (destination / 'local-stage.json').write_text(json.dumps(record, indent=2) + '\n')
    print(f"{stage.name}: {status} ({record['seconds']:.1f}s)", flush=True)
    return record


def output_run_index(output: Path) -> int:
    return json.loads((output / 'plan.json').read_text())['run_index']


async def execute(catalog: list[Stage], selected: set[str], output: Path,
                  root: Path, jobs: int, previous: dict[str, dict]) -> list[dict]:
    records = list(previous.values())
    build = next(stage for stage in catalog if stage.name == 'build')
    if 'build' in selected:
        records = [record for record in records if record['stage'] != 'build']
        records.append(await run_stage(build, output, root))
    built = any(record['stage'] == 'build' and record['result'] == 'success' for record in records)
    slots = asyncio.Semaphore(jobs)
    # Heavy stages have nested build concurrency; leave half the stage slots
    # for prebuilt tests and search. Never adjust Cargo's managed job budgets.
    heavy = asyncio.Semaphore(max(1, jobs // 2))

    async def consume(stage):
        if stage.needs_build and not built:
            return {'stage': stage.name, 'group': stage.group, 'result': 'skipped',
                    'seconds': 0, 'reason': 'shared build failed'}
        async def launch():
            async with slots:
                return await run_stage(stage, output, root)
        if stage.heavy:
            async with heavy:
                return await launch()
        return await launch()

    consumers = [stage for stage in catalog if stage.name in selected and stage.name != 'build']
    records.extend(await asyncio.gather(*(consume(stage) for stage in consumers)))
    for stage in catalog:
        if stage.name not in selected and stage.name not in previous:
            records.append({'stage': stage.name, 'group': stage.group, 'result': 'skipped',
                            'seconds': 0, 'reason': 'outside requested local selection'})
    return records


def evidence_files(output: Path):
    for directory, children, files in os.walk(output):
        children[:] = [name for name in children if name not in {'tmp', 'attempts'}]
        path = Path(directory)
        if path.name == 'mutants.out':
            children.clear()
            if 'missed.txt' in files:
                yield path / 'missed.txt'
            baseline = path / 'log/baseline.log'
            if baseline.is_file():
                yield baseline
        else:
            for name in files:
                if name.endswith('.log') and name != 'conclusion.log':
                    # Failed baselines are echoed into the process console.
                    # Their original baseline.log is the authoritative event.
                    if path == output / 'logs' and name.startswith('mutation-'):
                        continue
                    yield path / name


def summarize(root: Path, output: Path, records: list[dict], wall: float) -> int:
    catalog = stages(root, output_run_index(output), 'summary')
    groups = {}
    problems = []
    for group in sorted({stage.group for stage in catalog}):
        expected = {stage.name for stage in catalog if stage.group == group}
        actual = [record for record in records if record['group'] == group]
        results = [record['result'] for record in actual]
        complete = len(actual) == len(expected) and {record['stage'] for record in actual} == expected
        result = ('failure' if not complete or 'failure' in results else 'cancelled' if 'cancelled' in results
                  else 'skipped' if 'skipped' in results else 'success' if all(r == 'success' for r in results)
                  else 'failure')
        groups[group] = {'result': result}
        if result != 'success':
            problems.append(f'{group} ended with {result!r}, expected success')
    unknown = {record['group'] for record in records} - set(groups)
    if unknown:
        problems.append(f'Unknown stage groups: {sorted(unknown)}')
    (output / 'groups.json').write_text(json.dumps(groups, indent=2) + '\n')
    conclusion_code = int(bool(problems))
    conclusion_text = ('\n'.join(f'Confidence conclusion rejected: {problem}' for problem in problems)
                       if problems else 'Confidence conclusion accepted: every stage succeeded.') + '\n'
    (output / 'conclusion.log').write_text(conclusion_text)
    failed = []
    missed = []
    test_counts = {'passed': 0, 'failed': 0, 'ignored': 0}
    for path in sorted(evidence_files(output)):
        # A caught mutant's failing tests are expected. Report survivors using
        # cargo-mutants' authoritative missed.txt, not mutant log text.
        # Unmutated baseline logs are real test outcomes and remain evidence.
        if path.name == 'missed.txt':
            missed.extend(f'{path.relative_to(output)}: {line}'
                          for line in path.read_text().splitlines() if line.strip())
            continue
        for line in path.read_text(errors='replace').splitlines():
            if re.search(r'\b(FAILED|FAIL)\b', line):
                failed.append(f'{path.relative_to(output)}: {line}')
            counts = re.search(r'test result: .*? (\d+) passed; (\d+) failed; (\d+) ignored', line)
            if counts:
                for key, value in zip(test_counts, counts.groups()):
                    test_counts[key] += int(value)
            # nextest is preferred by the reused gate when installed.
            nextest = re.search(r'Summary\s+\[.*?\]\s+(\d+) tests? run:\s+(.*)', line)
            if nextest:
                for key in ('passed', 'failed'):
                    count = re.search(rf'(\d+) {key}\b', nextest[2])
                    if count:
                        test_counts[key] += int(count[1])
    summary = {'seconds': round(wall, 3), 'stages': records, 'groups': groups,
               'conclusion_exit_code': conclusion_code,
               'test_counts_from_logs': test_counts, 'failed_tests': failed, 'missed_mutants': missed}
    (output / 'summary.json').write_text(json.dumps(summary, indent=2) + '\n')
    lines = ['Confidence local stage summary', 'stage | result | seconds']
    lines.extend(f"{record['stage']} | {record['result']} | {record['seconds']:.1f}"
                 for record in records)
    lines.extend(['', 'FAILED tests:', *(failed or ['none recorded']), '',
                  'MISSED mutants:', *(missed or ['none recorded']), '',
                  f'Wall time: {wall:.1f}s', f'Test counts from logs: {test_counts}',
                  f'Conclusion exit: {conclusion_code}', conclusion_text.strip(), f'Artifacts: {output}'])
    text = '\n'.join(lines) + '\n'
    (output / 'summary.txt').write_text(text)
    print(text, end='', flush=True)
    return conclusion_code


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('-j', '--jobs', type=int, default=default_jobs(),
                        help='parallel stages (default: available CPUs and memory, max 8)')
    parser.add_argument('--only', action='append', default=[], metavar='STAGE-GLOB',
                        help='select stage names; repeat for a union; build is included for consumers')
    parser.add_argument('--skip-mutation', action='store_true',
                        help='omit mutation stages (the strict full conclusion remains incomplete)')
    parser.add_argument('--resume', action='store_true',
                        help='reuse completed stages in --out-dir; --only explicitly reruns matching stages')
    parser.add_argument('--out-dir', type=Path,
                        help='fresh evidence directory; inside the repo use .kiln (default: .kiln/confidence-local/<timestamp>)')
    parser.add_argument('--run-index', type=int,
                        default=int(os.environ.get('LASH_MUTATION_RUN_INDEX', '1')),
                        help='Local run index for rotating mutation slices (default: 1)')
    parser.add_argument('--list', action='store_true', help='list local stages without running them')
    args = parser.parse_args()
    if args.jobs < 1 or args.run_index < 1:
        parser.error('jobs and run-index must be positive')
    output = (args.out_dir or ROOT / '.kiln/confidence-local' / f'local-{time.time_ns()}').resolve()
    previous = {}
    prior_plan = None
    if args.resume:
        if not args.out_dir:
            parser.error('--resume requires --out-dir')
        prior_plan = json.loads((output / 'plan.json').read_text())
        if prior_plan['source_signature'] != source_signature(ROOT):
            parser.error('source inputs changed; use a fresh evidence directory')
        if prior_plan['run_index'] != args.run_index:
            parser.error('run-index differs from the evidence being resumed')
        previous = {record['stage']: record for record in
                    (json.loads(path.read_text()) for path in output.glob('*/local-stage.json'))}
    salt = prior_plan['salt'] if prior_plan else f'local-{time.time_ns()}'
    catalog = stages(ROOT, args.run_index, salt)
    selected = {stage.name for stage in catalog
                if (not args.only or any(fnmatch.fnmatchcase(stage.name, glob) for glob in args.only))
                and not (args.skip_mutation and stage.name.startswith('mutation-'))}
    if not selected:
        parser.error('no local stages match the selection')
    if any(stage.needs_build and stage.name in selected for stage in catalog):
        selected.add('build')
    if prior_plan:
        for name in list(selected):
            explicitly_selected = any(fnmatch.fnmatchcase(name, glob) for glob in args.only)
            if name in previous and not explicitly_selected:
                selected.remove(name)
            elif name in previous:
                previous.pop(name)
    if args.list:
        for stage in catalog:
            if stage.name in selected:
                print(f'{stage.name}\t{shlex.join(stage.command)}\t{json.dumps(stage.environment, sort_keys=True)}')
        return 0
    if not os.environ.get('KILN_GATE_ID'):
        parser.error('run scripts/confidence-local.sh, which enters kiln gate')
    if output.is_relative_to(ROOT) and not output.is_relative_to(ROOT / '.kiln'):
        parser.error('in-repository evidence must be under .kiln, outside Cargo workspace membership')
    output.mkdir(parents=True, exist_ok=True)
    if any(output.iterdir()) and not args.resume:
        parser.error(f'evidence directory is not empty: {output}')
    plan = {'revision': subprocess.check_output(['git', 'rev-parse', 'HEAD'], cwd=ROOT, text=True).strip(),
            'jobs': args.jobs, 'heavy_jobs': max(1, args.jobs // 2), 'run_index': args.run_index,
            'salt': salt, 'selected': sorted(selected), 'source_signature': source_signature(ROOT),
            'started_at': prior_plan['started_at'] if prior_plan else time.time()}
    (output / 'plan.json').write_text(json.dumps(plan, indent=2) + '\n')
    def interrupted(signum, frame):
        raise KeyboardInterrupt
    signal.signal(signal.SIGTERM, interrupted)
    try:
        records = asyncio.run(execute(catalog, selected, output, ROOT, args.jobs, previous))
    except KeyboardInterrupt:
        records = [json.loads(path.read_text()) for path in output.glob('*/local-stage.json')]
        present = {record['stage'] for record in records}
        records.extend({'stage': stage.name, 'group': stage.group, 'result': 'cancelled',
                        'seconds': 0, 'reason': 'local runner interrupted'}
                       for stage in catalog if stage.name not in present)
    return summarize(ROOT, output, records, time.time() - plan['started_at'])


if __name__ == '__main__':
    raise SystemExit(main())
