#!/usr/bin/env python3
"""Compile UI contracts with the declared Rust closure and compare stderr pins."""
import argparse
from concurrent.futures import ThreadPoolExecutor
import difflib
import json
import os
from pathlib import Path
import re
import subprocess
import tempfile


def filter_diagnostics(text, fixture, package):
    lines = []
    hide = False
    blanks = 0
    for line in text.splitlines():
        line = line.rstrip(' \t')
        if hide:
            first = line.lstrip()[:1]
            if first and (first.isdigit() or first in '|.'):
                lines.extend([''] * blanks)
                blanks = 0
                lines.append(re.sub(r'^[ 0-9]+', lambda m: ' ' * len(m[0]), line))
                continue
            hide = False
        if re.match(r'^(error: aborting due to |For more information about (this|an) error|Some errors have detailed explanations:|error: [Cc]ould not compile `)', line):
            continue
        if line == 'To learn more, run the command again with --verbose.':
            continue
        if line.startswith('= note: this compiler was built on 2') and line.endswith('consider upgrading it if it is out of date'):
            continue
        if re.match(r'^= note: the full (type )?name has been written to', line):
            continue
        line = re.sub(r'^(\s*and )\d+( others)$', r'\1$N\2', line)
        arrow = re.match(r'^(\s*(?:-->|:::) )(.*)$', line)
        if arrow:
            path = arrow[2]
            own = re.search(r'(?:[^ ]*/)?' + re.escape(package) + '/', path)
            workspace = re.search(r'(?:[^ ]*/)?crates/', path)
            registry = re.search(r'(?:[^ ]*/)?third-party/rust/(.+)-[0-9][^/]*\.crate/', path)
            if own:
                path = path[own.end():]
                hide = not path.startswith(fixture)
            elif workspace:
                path = '$WORKSPACE/crates/' + path[workspace.end():]
                hide = True
            elif registry:
                path = '$CARGO/' + registry[1] + '-$VERSION/' + path[registry.end():]
                hide = True
            if hide:
                path = re.sub(r':\d+:\d+$', '', path)
            line = arrow[1] + path
        if not line.strip():
            blanks += 1
            continue
        lines.extend([''] * blanks)
        blanks = 0
        lines.append(line)
    return lines


def line_kind(line, first=False, previous_note=False):
    if re.match(r'^(error|warning)[:\[]', line) or first and line.startswith('note: '):
        return 'H', 0
    if line.startswith(('note:', 'help:')) or line == '...' or previous_note and line.startswith('      '):
        return 'N', 0
    if line.startswith('... '):
        return 'C', len(line[4:]) - len(line[4:].lstrip(' '))
    match = re.match(r'^( *)([0-9]*)( *)(.*)$', line)
    spaces = len(match[1]) + len(match[3])
    digits = bool(match[2])
    rest = match[4]
    if spaces and (rest == '|' or rest.startswith('| ') or digits and (rest in ('~', '+', '-') or rest.startswith(('~ ', '+ ', '- '))) or not digits and rest.startswith(('--> ', '::: ', '= '))):
        return 'C', spaces - 1
    return 'O', 0 if digits else spaces


def normalize(text, fixture, package):
    lines = filter_diagnostics(text, fixture, package)
    output = []
    index = 0
    while index < len(lines):
        line = lines[index]
        heading = bool(re.match(r'^(error|warning)[:\[]|^note: ', line))
        kind, indent = line_kind(lines[index + 1]) if index + 1 < len(lines) else ('O', 0)
        if not heading or kind != 'C' or not lines[index + 1][indent + 1:].startswith('--> '):
            output.append(line)
            index += 1
            continue
        least = indent
        end = index + 2
        previous_note = False
        while end < len(lines):
            kind, spaces = line_kind(lines[end], previous_note=previous_note)
            if kind == 'H':
                break
            if kind == 'N':
                previous_note = True
            elif kind == 'C':
                previous_note = False
                least = min(least, spaces)
            elif kind == 'O' and spaces > 10:
                previous_note = False
            else:
                break
            end += 1
        output.append(line)
        previous_note = False
        for current in lines[index + 1:end]:
            kind, _ = line_kind(current, previous_note=previous_note)
            previous_note = kind == 'N'
            if kind in ('C', 'O'):
                start = current.find(' ')
                current = current[:start] + current[start + least:]
            output.append(current)
        index = end
    return '\n'.join(output) + ('\n' if output else '')


def overlay_sources(stage, destination, source):
    # Stage directories are always real directories. Following a directory
    # symlink while adding fixtures would write back into declared inputs.
    if not destination.is_relative_to(stage) or destination.is_symlink():
        raise ValueError('Unsafe UI fixture staging directory')
    destination.mkdir(parents=True, exist_ok=True)
    for entry in sorted(source.iterdir()):
        target = destination / entry.name
        if entry.is_dir():
            overlay_sources(stage, target, entry)
        elif not target.exists() and not target.is_symlink():
            target.symlink_to(entry.resolve())


def run_fixture(fixture, manifest, root, stage, output):
    name = fixture['name']
    source = root / fixture['source']
    expected_path = root / fixture['expected']
    case = output / name
    case.mkdir()
    flags = ['--edition=' + manifest['edition'], '--crate-type=bin', '--cfg', 'trybuild', '--color=never', '--emit=metadata', '--sysroot=' + str(root / manifest['sysroot'])]
    for directory in sorted({str((root / path).parent) for path in manifest['libraries']}):
        flags += ['-L', 'dependency=' + directory]
    for alias, artifact in sorted(manifest['externs'].items()):
        flags += ['--extern', alias + '=' + str(root / artifact)]
    command = [str(root / path) if index == 0 else path for index, path in enumerate(manifest['compiler'])]
    command += flags + ['--crate-name=' + name, '--out-dir', str(case), manifest['package'] + '/tests/ui/' + source.name]
    process = subprocess.run(command, cwd=stage, stdout=subprocess.PIPE, stderr=subprocess.STDOUT, text=True)
    actual = normalize(process.stdout, 'tests/ui/' + source.name, manifest['package'])
    expected = expected_path.read_text()
    (case / 'stderr.raw').write_text(process.stdout)
    (case / 'stderr.actual').write_text(actual)
    (case / 'stderr.expected').write_text(expected)
    difference = ''.join(difflib.unified_diff(expected.splitlines(keepends=True), actual.splitlines(keepends=True), fromfile=str(expected_path), tofile=name + '.actual'))
    (case / 'stderr.diff').write_text(difference)
    passed = process.returncode > 0 and actual == expected
    reason = 'rustc succeeded unexpectedly' if process.returncode == 0 else 'rustc terminated by signal' if process.returncode < 0 else 'diagnostic drift'
    return {'name': name, 'passed': passed, 'rustc_exit_code': process.returncode, 'reason': None if passed else reason, 'diff': difference}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument('--manifest', required=True, type=Path)
    args = parser.parse_args()
    root = Path.cwd()
    manifest = json.loads(args.manifest.read_text())
    jobs = int(os.environ.get('UI_JOBS', '8'))
    if jobs <= 0 or not manifest['fixtures']:
        parser.error('UI_JOBS must be positive and the fixture set must not be empty')
    output = Path(os.environ['TEST_UNDECLARED_OUTPUTS_DIR']).absolute() / 'ui-fixtures'
    output.mkdir(parents=True, exist_ok=True)
    with tempfile.TemporaryDirectory(prefix='lash-ui-') as temporary:
        stage = Path(temporary)
        for source in manifest['sources']:
            overlay_sources(stage, stage / source['package'], root / source['root'])
        fixture_dir = stage / manifest['package'] / 'tests/ui'
        fixture_dir.mkdir(parents=True, exist_ok=True)
        for fixture in manifest['fixtures']:
            destination = fixture_dir / Path(fixture['source']).name
            if destination.is_symlink() or destination.exists():
                destination.unlink()
            destination.symlink_to(root / fixture['source'])
        with ThreadPoolExecutor(max_workers=jobs) as pool:
            futures = [pool.submit(run_fixture, fixture, manifest, root, stage, output) for fixture in manifest['fixtures']]
            results = [future.result() for future in futures]
    print(f'running {len(results)} tests')
    for result in results:
        print('test ' + result['name'] + (' ... ok' if result['passed'] else ' ... FAILED'))
    failed = [result for result in results if not result['passed']]
    if failed:
        print('\nfailures:\n')
        for result in failed:
            print('---- ' + result['name'] + ' stdout ----')
            print(result['reason'])
            print(result['diff'])
        print('failures:')
        for result in failed:
            print('    ' + result['name'])
    (output / 'results.json').write_text(json.dumps({'schema': 1, 'results': results}, indent=2) + '\n')
    failures = len(failed)
    print(f'test result: {"FAILED" if failures else "ok"}. {len(results) - failures} passed; {failures} failed; 0 ignored')
    return 1 if failures else 0


if __name__ == '__main__':
    raise SystemExit(main())
