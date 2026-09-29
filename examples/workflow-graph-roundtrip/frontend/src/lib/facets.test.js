import { describe, it, expect } from 'vitest';
import {
  hasFacets,
  parseType,
  enumMembers,
  enumMemberToText,
  enumMemberFromText,
  describeExpectedType,
  typesCompatible,
  normalizeVars,
  varNames,
  compatibleVarNames,
  expectedSlotType,
  expectedArgFieldType,
  nodeDiagnostics,
  mapDiagnosticsToNodes,
  documentDiagnostics,
  clearFacetDiagnostics,
  diagnosticFieldHint,
} from './facets.js';

// Facet shapes below mirror the live backend samples captured from
// GET /workflow and POST /project (see contract.rs): availableVars carry a
// `type`, expectedArgTypes carry a `slot`, diagnostics carry a `nodeId`.

describe('hasFacets — graceful degradation gate', () => {
  it('is true only when facetSchemaVersion is present', () => {
    expect(hasFacets({ facetSchemaVersion: 1, nodes: [] })).toBe(true);
    expect(hasFacets({ facetSchemaVersion: 0, nodes: [] })).toBe(true);
    expect(hasFacets({ nodes: [] })).toBe(false);
    expect(hasFacets(null)).toBe(false);
  });
});

describe('parseType — lens format_type_expr spelling', () => {
  it('parses scalars, list, enum, object, union, ref', () => {
    expect(parseType('any')).toEqual({ kind: 'any' });
    expect(parseType('dict')).toEqual({ kind: 'dict' });
    expect(parseType('int')).toEqual({ kind: 'int' });
    expect(parseType('float')).toEqual({ kind: 'float' });
    expect(parseType('str')).toEqual({ kind: 'str' });
    expect(parseType('bool')).toEqual({ kind: 'bool' });
    expect(parseType('list[int]')).toEqual({ kind: 'list', item: { kind: 'int' } });
    expect(parseType('list[list[str]]')).toEqual({
      kind: 'list',
      item: { kind: 'list', item: { kind: 'str' } },
    });
    expect(parseType('{ text: str }')).toEqual({ kind: 'object' });
    expect(parseType('str | float | bool')).toEqual({
      kind: 'union',
      members: [{ kind: 'str' }, { kind: 'float' }, { kind: 'bool' }],
    });
    expect(parseType('MyAlias')).toEqual({ kind: 'ref', name: 'MyAlias' });
  });

  it('does not split a union sign nested inside brackets', () => {
    // A list of a union stays one list, not a spurious top-level union.
    expect(parseType('list[str | int]')).toEqual({
      kind: 'list',
      item: { kind: 'union', members: [{ kind: 'str' }, { kind: 'int' }] },
    });
  });
});

describe('enumMembers', () => {
  it('extracts unquoted members from an enum type', () => {
    expect(enumMembers('enum["ok", "warn", "err"]')).toEqual(['ok', 'warn', 'err']);
    expect(enumMembers('enum[]')).toEqual([]);
  });

  it('returns null for non-enum types', () => {
    expect(enumMembers('str')).toBeNull();
    expect(enumMembers('list[str]')).toBeNull();
    expect(enumMembers('')).toBeNull();
  });

  it('does not confuse an enum member comma for a separator', () => {
    expect(enumMembers('enum["a, b", "c"]')).toEqual(['a, b', 'c']);
  });
});

describe('enum dropdown selection', () => {
  const members = ['ok', 'warn', 'err'];

  it('encodes a chosen member as canonical quoted text', () => {
    expect(enumMemberToText('ok')).toBe('"ok"');
  });

  it('selects the member from a quoted expression slot', () => {
    expect(enumMemberFromText('"warn"', members)).toBe('warn');
    expect(enumMemberFromText('  "err" ', members)).toBe('err');
  });

  it('selects the member from a bare string field value', () => {
    expect(enumMemberFromText('ok', members)).toBe('ok');
  });

  it('returns null for a value outside the members (custom / variable)', () => {
    expect(enumMemberFromText('"nope"', members)).toBeNull();
    expect(enumMemberFromText('someVar', members)).toBeNull();
    expect(enumMemberFromText('', members)).toBeNull();
  });

  it('round-trips member -> text -> member', () => {
    for (const m of members) {
      expect(enumMemberFromText(enumMemberToText(m), members)).toBe(m);
    }
  });
});

describe('describeExpectedType — subtle slot hint', () => {
  it('hides gradual / absent types', () => {
    expect(describeExpectedType('any')).toBeNull();
    expect(describeExpectedType('dict')).toBeNull();
    expect(describeExpectedType('')).toBeNull();
    expect(describeExpectedType(null)).toBeNull();
  });

  it('simplifies the structural list[any] to list', () => {
    expect(describeExpectedType('list[any]')).toBe('list');
  });

  it('passes concrete types through', () => {
    expect(describeExpectedType('str')).toBe('str');
    expect(describeExpectedType('str | float | bool')).toBe('str | float | bool');
  });
});

describe('typesCompatible — slot filter with gradual Any/Dict', () => {
  it('an empty / missing expected type offers everything', () => {
    expect(typesCompatible('str', '')).toBe(true);
    expect(typesCompatible('str', null)).toBe(true);
  });

  it('an Any or Dict expected slot offers everything', () => {
    expect(typesCompatible('str', 'any')).toBe(true);
    expect(typesCompatible('list[int]', 'any')).toBe(true);
    expect(typesCompatible('int', 'dict')).toBe(true);
  });

  it('an Any or Dict typed var is always offered', () => {
    expect(typesCompatible('any', 'str')).toBe(true);
    expect(typesCompatible('any', 'list[int]')).toBe(true);
    expect(typesCompatible('dict', 'str')).toBe(true);
  });

  it('matches exact scalar kinds and rejects mismatches', () => {
    expect(typesCompatible('str', 'str')).toBe(true);
    expect(typesCompatible('bool', 'bool')).toBe(true);
    expect(typesCompatible('str', 'bool')).toBe(false);
    expect(typesCompatible('str', 'int')).toBe(false);
  });

  it('treats int and float as interchangeable numerics', () => {
    expect(typesCompatible('int', 'float')).toBe(true);
    expect(typesCompatible('float', 'int')).toBe(true);
    expect(typesCompatible('int', 'str')).toBe(false);
  });

  it('honors unions on either side', () => {
    expect(typesCompatible('float', 'str | float | bool')).toBe(true);
    expect(typesCompatible('int', 'str | float | bool')).toBe(true); // int~float
    expect(typesCompatible('list[int]', 'str | float | bool')).toBe(false);
    expect(typesCompatible('str | int', 'int')).toBe(true);
  });

  it('matches lists structurally and gradually', () => {
    expect(typesCompatible('list[int]', 'list[int]')).toBe(true);
    expect(typesCompatible('list[int]', 'list[float]')).toBe(true); // numeric item
    expect(typesCompatible('list[any]', 'list[str]')).toBe(true); // gradual item
    expect(typesCompatible('list[str]', 'list[int]')).toBe(false);
    expect(typesCompatible('str', 'list[str]')).toBe(false); // scalar into a list slot
    expect(typesCompatible('list[str]', 'str')).toBe(false);
  });

  it('treats enum as a refinement of str', () => {
    expect(typesCompatible('str', 'enum["a", "b"]')).toBe(true);
    expect(typesCompatible('enum["a", "b"]', 'str')).toBe(true);
    expect(typesCompatible('int', 'enum["a", "b"]')).toBe(false);
  });

  it('matches nominal refs only by name', () => {
    expect(typesCompatible('Widget', 'Widget')).toBe(true);
    expect(typesCompatible('Widget', 'Gadget')).toBe(false);
  });
});

describe('normalizeVars / varNames / compatibleVarNames', () => {
  const vars = [
    { name: 'approval', type: 'any' },
    { name: 'count', type: 'float' },
    { name: 'label', type: 'str' },
    { name: 'items', type: 'list[str]' },
  ];

  it('normalizes typed and legacy names-only shapes', () => {
    expect(normalizeVars(vars)).toEqual(vars);
    expect(normalizeVars(['a', 'b'])).toEqual([
      { name: 'a', type: 'any' },
      { name: 'b', type: 'any' },
    ]);
    expect(normalizeVars(null)).toEqual([]);
  });

  it('lists every name unfiltered', () => {
    expect(varNames(vars)).toEqual(['approval', 'count', 'label', 'items']);
  });

  it('offers only numeric-compatible vars for a numeric slot (plus gradual Any)', () => {
    // `count` (float) matches; `approval` (any) is always offered; str/list drop.
    expect(compatibleVarNames(vars, 'float')).toEqual(['approval', 'count']);
    expect(compatibleVarNames(vars, 'int')).toEqual(['approval', 'count']);
  });

  it('offers only str-compatible vars for a str slot', () => {
    expect(compatibleVarNames(vars, 'str')).toEqual(['approval', 'label']);
  });

  it('offers only list-typed vars for a list iterable slot (plus gradual Any)', () => {
    expect(compatibleVarNames(vars, 'list[str]')).toEqual(['approval', 'items']);
    // The structural `list[any]` we pass for `for`/comprehension iterables.
    expect(compatibleVarNames(vars, 'list[any]')).toEqual(['approval', 'items']);
  });

  it('offers everything when no expected type is known', () => {
    expect(compatibleVarNames(vars, null)).toEqual(['approval', 'count', 'label', 'items']);
    expect(compatibleVarNames(vars, 'any')).toEqual(['approval', 'count', 'label', 'items']);
  });
});

describe('expectedSlotType / expectedArgFieldType', () => {
  const node = {
    data: {
      expectedArgTypes: [
        { slot: 'arg[0]', type: '{ name: str, state: str | float | bool }' },
        { slot: 'arg[0]["name"]', type: 'str' },
        { slot: 'arg[0]["state"]', type: 'str | float | bool' },
      ],
    },
  };
  const multi = {
    data: {
      expectedArgTypes: [
        { slot: 'call[0].arg[0]["pct"]', type: 'float' },
        { slot: 'call[1].arg[0]["text"]', type: 'str' },
      ],
    },
  };

  it('resolves an exact slot path', () => {
    expect(expectedSlotType(node, 'arg[0]')).toBe('{ name: str, state: str | float | bool }');
    expect(expectedSlotType(node, 'arg[0]["name"]')).toBe('str');
    expect(expectedSlotType(node, 'missing')).toBeNull();
  });

  it('resolves a record-arg field by name (single-call canonical slot)', () => {
    expect(expectedArgFieldType(node, 'name')).toBe('str');
    expect(expectedArgFieldType(node, 'state')).toBe('str | float | bool');
    expect(expectedArgFieldType(node, 'nope')).toBeNull();
  });

  it('falls back to a slot tail for multi-call nodes', () => {
    expect(expectedArgFieldType(multi, 'pct')).toBe('float');
    expect(expectedArgFieldType(multi, 'text')).toBe('str');
  });
});

describe('diagnostics — mapping and stale facet clearing', () => {
  const badIterable = {
    facetSchemaVersion: 1,
    nodes: [
      { id: 'call:a', data: { kind: 'call', diagnostics: [] } },
      {
        id: 'container:for1',
        data: {
          kind: 'container',
          subkind: 'for',
          diagnostics: [
            {
              nodeId: 'container:for1',
              kind: 'incompatible_iteration_target',
              message: 'cannot iterate over str; expected a list',
              span: { start: 0, end: 95 },
            },
          ],
        },
      },
    ],
  };
  const clean = {
    facetSchemaVersion: 1,
    nodes: [{ id: 'call:a', data: { kind: 'call', diagnostics: [] } }],
  };
  const legacy = {
    // no facetSchemaVersion — older backend
    nodes: [{ id: 'call:a', data: { kind: 'call' } }],
  };

  it('reads a node’s own diagnostics', () => {
    expect(nodeDiagnostics(badIterable.nodes[1])).toHaveLength(1);
    expect(nodeDiagnostics(badIterable.nodes[0])).toEqual([]);
    expect(nodeDiagnostics({ data: {} })).toEqual([]);
  });

  it('maps each diagnostic to its owning node id', () => {
    const byNode = mapDiagnosticsToNodes(badIterable);
    expect([...byNode.keys()]).toEqual(['container:for1']);
    expect(byNode.get('container:for1')[0].kind).toBe('incompatible_iteration_target');
  });

  it('collects type errors only when facets are present', () => {
    expect(documentDiagnostics(badIterable)).toEqual([
      {
        nodeId: 'container:for1',
        kind: 'incompatible_iteration_target',
        message: 'cannot iterate over str; expected a list',
      },
    ]);
    expect(documentDiagnostics(clean)).toEqual([]);
    expect(documentDiagnostics(legacy)).toEqual([]);
  });

  it('hints the offending field for well-known diagnostic kinds', () => {
    expect(diagnosticFieldHint('incompatible_iteration_target')).toBe('iterable');
    expect(diagnosticFieldHint('unknown_resource_operation')).toBeNull();
  });

  it('clears stale diagnostics on edit', () => {
    const doc = structuredClone(badIterable);
    expect(documentDiagnostics(doc)).toHaveLength(1);
    expect(clearFacetDiagnostics(doc)).toBe(true);
    expect(documentDiagnostics(doc)).toEqual([]);
    // Idempotent: nothing left to clear.
    expect(clearFacetDiagnostics(doc)).toBe(false);
  });
});
