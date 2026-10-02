#!/usr/bin/env python3
"""Discover source-declared versioned surfaces and refuse undeclared versions.

A constant declares its upgrade policy with `/// version_surface = "coexist"`
and its grammar with the existing `version_guard` marker. No registry row is
needed. `format_manifest` names a DurableFormat variant or engine:<id>;
`format_outside_manifest` states why a surface is not in those manifests.
`version_unguarded` explains a migrate surface outside the decoder-law table.
Identity and key grammars without manifest markers are outside the manifest.
String versions must be scalar constants so the cut can reset them.
Reservation arrays may declare `version_reservations = "<reason>"`; those
historical names are immutable and are not current writer versions.
"""
from __future__ import annotations

from functools import lru_cache
import re
import tomllib

CONST = re.compile(
    r'(?m)^[ \t]*(?:pub(?:\([^)]*\))?\s+)?const\s+(?P<name>[A-Z][A-Z0-9_]*)'
    r'\s*:\s*(?P<type>[^=;]+)=\s*(?P<value>[^;]+);'
)
STRING = re.compile(r'r(?P<hashes>\#*)".*?"(?P=hashes)|"(?:\\.|[^"\\])*"', re.DOTALL)
TAG = re.compile(r'^[A-Za-z][A-Za-z0-9_.:/-]*?[/:-]v[0-9]+(?:[/:-][A-Za-z0-9_./:{}-]*)?$')
DECLARATION = re.compile(
    r'(?m)^\s*///\s*(version_surface|format_manifest|format_outside_manifest|version_unguarded)'
    r'\s*=\s*(.*?)\s*$'
)
RESERVATIONS = re.compile(r'(?m)^\s*///\s*version_reservations\s*=\s*"([^"\n]+)"\s*$')


def production_path(path):
    import check_version_bumps as gate
    parts = path.split('/')
    return (not gate._is_test_source(path)
            and not any(part.endswith('_tests') or part == 'testing' for part in parts)
            and not path.startswith(('crates/lash-conformance/', 'crates/lash-sim/', 'crates/lash-perf/'))
            and '/examples/' not in path and '/benches/' not in path)


def production_text(text):
    """Mask comments, test modules and attributes, preserving byte offsets."""
    import check_version_bumps as gate
    import release_baseline as baseline
    attributes = gate.rust_outer_attribute_ranges(text)
    masked = list(baseline.without_comments(text))
    for start, end in (*attributes, *gate.test_only_module_ranges(text, attributes)):
        masked[start:end] = ' ' * (end - start)
    return ''.join(masked)


def discover(view, *, unregistered=None, enforce=True):
    import check_version_bumps as gate
    registry = tomllib.loads(view.content(gate.REGISTRY) or '')
    if 'surface' in registry:
        raise gate.CheckError('[[surface]] declarations are unsupported; declare version_surface in source')
    excluded = {f'{row["constant_path"]}:{row["constant"]}'
                for row in registry.get('unregistered', [])}
    if unregistered is not None:
        excluded = set(unregistered)
    excluded = frozenset(excluded)
    rows, problems = {}, []
    paths = view.matching_paths(gate.RUST_SOURCE_PATTERNS)
    view.preload(paths)
    for path in paths:
        found, errors = _discover_file(path, view.content(path), excluded)
        for row in found:
            key = f'{row["constant_path"]}:{row["constant"]}'
            if key in rows and rows[key] != row:
                problems.append(f'{key}: conflicting source declarations')
            rows[key] = row.copy()
        problems.extend(errors)
    if enforce and problems:
        raise gate.CheckError('\n'.join(problems))
    return tuple(rows.values()), problems


@lru_cache(maxsize=8192)
def _discover_file(path, text, excluded):
    # Mutant trees used by the guard laws change one file. Reuse the scan of
    # each unchanged file while still scanning every tree for new versions.
    import check_version_bumps as gate
    rows, problems = {}, []
    masked = production_text(text)
    constants = list(CONST.finditer(masked))
    attributes = gate.rust_outer_attribute_ranges(text)

    def block_for(match):
        start = gate.rust_item_start_with_attributes(text, match.start(), attributes)
        while start > 0:
            line_start = text.rfind("\n", 0, start - 1) + 1
            if not text[line_start:start].lstrip().startswith("//"):
                break
            start = line_start
        return text[start:match.start()]

    reserved = []
    for match in constants:
        name = match['name']
        key = f'{path}:{name}'
        if match['value'].strip() == 'env!("CARGO_PKG_VERSION")':
            continue
        block = block_for(match)
        metadata = {}
        invalid = False
        for marker in DECLARATION.finditer(block):
            field, expression = marker.groups()
            try:
                value = tomllib.loads('value = ' + expression)['value']
                if not isinstance(value, str) or not value.strip():
                    raise ValueError('expected a non-empty string')
                if field == 'version_surface' and value not in ('migrate', 'drain', 'coexist'):
                    raise ValueError('expected migrate, drain or coexist')
                if field in metadata and metadata[field] != value:
                    raise ValueError('conflicting source declarations')
                metadata[field] = value
            except (tomllib.TOMLDecodeError, KeyError, ValueError) as error:
                problems.append(f'{key}: invalid {field}: {error}')
                invalid = True
        if invalid:
            continue
        reasons = [m[1] for m in RESERVATIONS.finditer(block)]
        if reasons:
            if match['type'].strip() != '&[&str]' or not all(r.strip() for r in reasons):
                problems.append(f'{key}: version reservations must name a string array and a reason')
            else:
                reserved.append(match.span('value'))
            continue
        if 'version_surface' in metadata:
            row = dict(constant_path=path, constant=name, upgrade=metadata['version_surface'])
            for marker, field in (('format_manifest', 'manifest'),
                                  ('format_outside_manifest', 'outside_manifest'),
                                  ('version_unguarded', 'unguarded')):
                if marker in metadata:
                    row[field] = metadata[marker]
            if not any(field in row for field in ('manifest', 'outside_manifest')):
                row['outside_manifest'] = 'source-declared identity or key grammar'
            if key in rows and rows[key] != row:
                problems.append(f'{key}: conflicting source declarations')
            rows[key] = row
        elif metadata:
            problems.append(f'{key}: format metadata needs version_surface')
        elif (production_path(path) and re.search(r'(?:^|_)(?:VERSION|EPOCH|FORMAT|V[0-9]+)$', name) and key not in excluded):
            problems.append(f'{key} is a version-shaped constant the registry does not know: '
                            'declare version_surface and version_guard')
    if not production_path(path):
        return tuple(rows.values()), tuple(problems)
    for call in re.finditer(
        r'\b(?:IdentityEncoder::new|rendered_hash)\(\s*"(?:\\.|[^"\\])*"\s*,\s*([0-9][0-9_]*(?:u8|u16|u32)?)\b',
        masked,
    ):
        problems.append(f'{path}:{text.count(chr(10), 0, call.start()) + 1}: '
                        f'unregistered inline family version {call[1]}; use a source-declared constant')
    owned = rows.keys() | excluded
    for literal in STRING.finditer(masked):
        raw = literal.group()
        value = raw[raw.index('"') + 1:raw.rindex('"')]
        if (not TAG.fullmatch(value) or value.startswith(('http:', 'https:'))
                or not (re.search(r'[/ :]v[0-9]', value) or value.startswith(('lash', 'restate-authority')))):
            continue
        if any(start <= literal.start() < end for start, end in reserved):
            continue
        owner = next((m for m in constants if m.start('value') <= literal.start() < m.end('value')), None)
        if owner is not None and f'{path}:{owner["name"]}' in owned:
            # A scalar string can be resolved and reset. Arrays of active
            # writer tags cannot hide behind one numeric counter.
            if owner['value'].strip() == raw:
                continue
        problems.append(f'{path}:{text.count(chr(10), 0, literal.start()) + 1}: '
                        f'unregistered version literal {value!r}; use a source-declared scalar constant')
    return tuple(rows.values()), tuple(problems)

def surfaces(view, *, enforce=True):
    import check_version_bumps as gate
    text = view.content(gate.REGISTRY)
    if text is None:
        raise gate.CheckError(f'{view.label}: cannot read {gate.REGISTRY}')
    discovered, _ = discover(view, enforce=enforce)
    return tuple(gate.surface_of(row, row['constant']) for row in discovered)
