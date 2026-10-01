"""Keep manual tests explicit and private Rust binaries out of managed tests."""
import json
import re
import subprocess
import sys


LABEL = re.compile(r'^(?:[A-Za-z0-9_.-]+)?//')
RELATIVE = re.compile(r'^[A-Za-z0-9_./-]*:[A-Za-z0-9_.+*\[\]-]*$')
VALUE_FLAGS = {
    '--target-platforms', '--modifier', '--modifiers', '--build-report',
    '--event-log', '--write-build-id', '--command-report-path', '--config-file',
    '--profile-patterns', '--oncall', '--client-metadata', '--agent-context',
    '-c', '--config', '-a', '--target-universe',
}


def canonical(label):
    if label.startswith('//'):
        return 'root' + label
    return label if LABEL.match(label) else 'root//' + label.removeprefix('./')


def wildcard(label):
    return label == '...' or label.endswith(('/...', ':', ':all', ':*'))


def skipped_line(labels, variants=0):
    """The one line that keeps a pattern's dropped manual targets visible."""
    names = sorted(labels) + ([f'{variants} feature-lane variants'] if variants else [])
    return 'hermetic-build: patterns skip manual targets; name one to run it. Skipped: ' + ' '.join(names)


def query_targets(argv, root, pattern):
    if pattern.endswith((':all', ':*')):
        pattern = pattern.rsplit(':', 1)[0] + ':'
    result = subprocess.run(
        argv[:3] + ['uquery', '--json', '-a', '^(labels|tags)$', pattern],
        cwd=root, capture_output=True, text=True, check=True, timeout=30,
    )
    targets = json.loads(result.stdout)
    if not isinstance(targets, dict):
        raise ValueError('Buck2 test pattern query returned an invalid target map')
    for label, attrs in targets.items():
        if not isinstance(label, str) or not LABEL.match(label) or not isinstance(attrs, dict):
            raise ValueError('Buck2 test pattern query returned an invalid target')
        for field in ('labels', 'tags'):
            values = attrs.get(field, [])
            if not isinstance(values, list) or not all(isinstance(value, str) for value in values):
                raise ValueError('Managed wildcard tests require static labels and tags')
    return targets


def target_positions(tokens, start=0):
    positions = []
    skip = False
    for index in range(start, len(tokens)):
        value = tokens[index]
        if skip:
            skip = False
        elif value in VALUE_FLAGS:
            skip = True
        elif LABEL.match(value) or RELATIVE.fullmatch(value) or (not value.startswith('-') and (value == '...' or value.endswith('/...'))):
            positions.append(index)
    return positions


def plan_test_command(argv, root):
    boundary = argv.index('--')
    front = argv[:boundary]
    positions = target_positions(front, 4)
    if not any(wildcard(front[index]) for index in positions):
        return argv
    chosen = set()
    expanded = {}
    skipped = {}
    for index in positions:
        token = front[index]
        if wildcard(token):
            targets = query_targets(argv, root, token)
            policy = {label: attrs.get('labels', []) + attrs.get('tags', []) for label, attrs in targets.items()}
            labels = sorted(label for label, tags in policy.items() if 'manual' not in tags)
            skipped.update((label, 'feature-lane' in tags) for label, tags in policy.items() if 'manual' in tags)
        else:
            labels = [token]
        expanded[index] = []
        for label in labels:
            key = canonical(label)
            if key not in chosen:
                chosen.add(key)
                expanded[index].append(label)
    named = sorted(label.removeprefix('root') for label, variant in skipped.items() if not variant and label not in chosen)
    variants = sum(variant for label, variant in skipped.items() if label not in chosen)
    if named or variants:
        print(skipped_line(named, variants), file=sys.stderr, flush=True)
    if not chosen:
        raise ValueError('No non-manual targets matched; name a manual target explicitly to run it')
    output = []
    for index, value in enumerate(front):
        output.extend(expanded.get(index, [value]))
    return output + argv[boundary:]
