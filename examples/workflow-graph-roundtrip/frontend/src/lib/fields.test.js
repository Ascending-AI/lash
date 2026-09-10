import { describe, it, expect } from 'vitest';
import {
  parseLiteral,
  encodeLiteral,
  parseList,
  encodeList,
  parseComparison,
  stripOuterParens,
  isBoolLiteral,
  isSimpleReference,
  isComplexExpression,
  operandType,
  clauseAdded,
  clauseRemoved,
  makeClause,
} from './fields.js';

describe('literal parse/encode', () => {
  it('classifies scalar literals', () => {
    expect(parseLiteral('42')).toEqual({ type: 'number', value: '42' });
    expect(parseLiteral('-3.5')).toEqual({ type: 'number', value: '-3.5' });
    expect(parseLiteral('true')).toEqual({ type: 'boolean', value: 'true' });
    expect(parseLiteral('"hi"')).toEqual({ type: 'string', value: 'hi' });
    expect(parseLiteral('')).toEqual({ type: 'string', value: '' });
  });

  it('treats non-literals as expression (stay raw)', () => {
    expect(parseLiteral('a + b').type).toBe('expression');
    expect(parseLiteral('foo(1)').type).toBe('expression');
  });

  it('encodes canonical Lashlang literals', () => {
    expect(encodeLiteral('number', '7')).toBe('7');
    expect(encodeLiteral('number', '')).toBe('0');
    expect(encodeLiteral('boolean', 'true')).toBe('true');
    expect(encodeLiteral('boolean', 'false')).toBe('false');
    expect(encodeLiteral('string', 'done')).toBe('"done"');
    expect(encodeLiteral('string', 'a"b')).toBe('"a\\"b"');
  });

  it('round-trips plain scalar literals through parse -> encode', () => {
    for (const raw of ['"hello"', '"done"', '42', '-3.5', 'true', 'false']) {
      const lit = parseLiteral(raw);
      expect(encodeLiteral(lit.type, lit.value)).toBe(raw);
    }
  });
});

describe('list parse/encode', () => {
  it('parses a flat scalar list', () => {
    expect(parseList('[1, 2, 3]')).toEqual({
      items: [
        { type: 'number', value: '1' },
        { type: 'number', value: '2' },
        { type: 'number', value: '3' },
      ],
    });
  });

  it('parses an empty list', () => {
    expect(parseList('[]')).toEqual({ items: [] });
  });

  it('parses mixed scalar types with quoted commas', () => {
    const parsed = parseList('["a, b", true, 5]');
    expect(parsed).toEqual({
      items: [
        { type: 'string', value: 'a, b' },
        { type: 'boolean', value: 'true' },
        { type: 'number', value: '5' },
      ],
    });
  });

  it('rejects non-flat / nested / expression lists', () => {
    expect(parseList('[a + b]')).toBeNull();
    expect(parseList('[[1], 2]')).toBeNull();
    expect(parseList('not a list')).toBeNull();
    expect(parseList('[1, 2')).toBeNull();
  });

  it('encodes scalar items to canonical list text', () => {
    expect(
      encodeList([
        { type: 'number', value: '1' },
        { type: 'string', value: 'x' },
      ]),
    ).toBe('[1, "x"]');
    expect(encodeList([])).toBe('[]');
  });

  it('round-trips list text through parse -> encode', () => {
    for (const raw of ['[1, 2, 3]', '["a", "b"]', '[true, false]', '[]']) {
      expect(encodeList(parseList(raw).items)).toBe(raw);
    }
  });
});

describe('comparison parse', () => {
  it('parses a simple comparison', () => {
    expect(parseComparison('count < 10')).toEqual({ lhs: 'count', op: '<', rhs: '10' });
    expect(parseComparison('name == "bob"')).toEqual({ lhs: 'name', op: '==', rhs: '"bob"' });
    expect(parseComparison('a >= b')).toEqual({ lhs: 'a', op: '>=', rhs: 'b' });
  });

  it('round-trips a comparison through parse -> rebuild', () => {
    const src = 'total != 0';
    const { lhs, op, rhs } = parseComparison(src);
    expect(`${lhs} ${op} ${rhs}`).toBe(src);
  });

  it('rejects chained / compound comparisons (stay raw)', () => {
    expect(parseComparison('a < b < c')).toBeNull();
    expect(parseComparison('plain text')).toBeNull();
  });
});

describe('parenthesized conditions (the lens emits these)', () => {
  it('strips a balanced outer paren pair', () => {
    expect(stripOuterParens('(x < 2)')).toBe('x < 2');
    expect(stripOuterParens('((a))')).toBe('a');
    expect(stripOuterParens('  (state.count < 3)  ')).toBe('state.count < 3');
  });

  it('leaves non-wrapping parens intact', () => {
    expect(stripOuterParens('(a) < (b)')).toBe('(a) < (b)');
    expect(stripOuterParens('x < 2')).toBe('x < 2');
    expect(stripOuterParens('(a) && (b)')).toBe('(a) && (b)');
  });

  it('decodes a lens-emitted `(x < 2)` to clean operands', () => {
    expect(parseComparison('(x < 2)')).toEqual({ lhs: 'x', op: '<', rhs: '2' });
  });

  it('decodes `(state.count < 3)` with a dotted lhs', () => {
    expect(parseComparison('(state.count < 3)')).toEqual({
      lhs: 'state.count',
      op: '<',
      rhs: '3',
    });
  });

  it('re-encodes bare (lens re-wraps) so a round-trip stays balanced', () => {
    const parsed = parseComparison('(x < 2)');
    const emitted = `${parsed.lhs} ${parsed.op} ${parsed.rhs}`;
    expect(emitted).toBe('x < 2'); // bare — no stray parens, balanced
    // and re-parsing the wrapped form the lens would produce is stable
    expect(parseComparison(`(${emitted})`)).toEqual(parsed);
  });

  it('does NOT mis-parse a compound expression as a comparison', () => {
    // Naive splitting would yield lhs="(x" — must reject and fall back to raw.
    expect(parseComparison('(x < 2) && ok')).toBeNull();
  });
});

describe('variable-vs-variable comparisons', () => {
  it('decodes both operands as bare references', () => {
    expect(parseComparison('count < limit')).toEqual({ lhs: 'count', op: '<', rhs: 'limit' });
    expect(parseComparison('(a < b)')).toEqual({ lhs: 'a', op: '<', rhs: 'b' });
  });

  it('re-encodes bare and round-trips stably through the lens wrap', () => {
    const parsed = parseComparison('(count < limit)');
    const emitted = `${parsed.lhs} ${parsed.op} ${parsed.rhs}`;
    expect(emitted).toBe('count < limit'); // bare, valid, balanced
    expect(parseComparison(`(${emitted})`)).toEqual(parsed);
  });

  it('classifies each RHS operand as var vs literal against scope', () => {
    const vars = ['count', 'limit'];
    // RHS is an in-scope variable → the RHS builder shows it as a variable.
    expect(operandType(parseComparison('count < limit').rhs, vars)).toBe('var');
    // RHS is a literal → the RHS builder shows a scalar.
    expect(operandType(parseComparison('count < 3').rhs, vars)).toBe('number');
    // A variable not in scope is not offered as a pick (stays literal/expression).
    expect(operandType('missing', vars)).toBe('expression');
  });

  it('decodes a literal-LHS comparison (0 < count) with both operands typed', () => {
    const vars = ['count'];
    const parsed = parseComparison('0 < count');
    expect(parsed).toEqual({ lhs: '0', op: '<', rhs: 'count' });
    // LHS is a scalar literal, RHS an in-scope variable — both operand builders
    // populate (neither is empty).
    expect(operandType(parsed.lhs, vars)).toBe('number');
    expect(operandType(parsed.rhs, vars)).toBe('var');
  });

  it('decodes a var-LHS comparison (count < 3) with both operands typed', () => {
    const vars = ['count'];
    const parsed = parseComparison('(count < 3)');
    expect(parsed).toEqual({ lhs: 'count', op: '<', rhs: '3' });
    expect(operandType(parsed.lhs, vars)).toBe('var');
    expect(operandType(parsed.rhs, vars)).toBe('number');
  });

  it('re-encodes a literal-LHS comparison bare and round-trips through the wrap', () => {
    const parsed = parseComparison('(0 < count)');
    const emitted = `${parsed.lhs} ${parsed.op} ${parsed.rhs}`;
    expect(emitted).toBe('0 < count');
    expect(parseComparison(`(${emitted})`)).toEqual(parsed);
  });
});

describe('operandType — var-vs-scalar classification for the value builder', () => {
  const vars = ['x', 'y', 'state.count'];

  it('picks an in-scope variable (bare or dotted) when it matches scope', () => {
    expect(operandType('y', vars)).toBe('var');
    expect(operandType('state.count', vars)).toBe('var');
    expect(operandType('  x  ', vars)).toBe('var');
  });

  it('classifies scalar literals by kind', () => {
    expect(operandType('42', vars)).toBe('number');
    expect(operandType('"hi"', vars)).toBe('string');
    expect(operandType('true', vars)).toBe('boolean');
  });

  it('leaves compound / out-of-scope values as expression (raw)', () => {
    expect(operandType('a + b', vars)).toBe('expression');
    expect(operandType('compute(x)', vars)).toBe('expression');
    expect(operandType('notInScope', vars)).toBe('expression');
    expect(operandType('y', [])).toBe('expression'); // no scope supplied
  });
});

describe('predicates', () => {
  it('detects bare boolean literals', () => {
    expect(isBoolLiteral('true')).toBe(true);
    expect(isBoolLiteral(' false ')).toBe(true);
    expect(isBoolLiteral('x')).toBe(false);
  });

  it('detects simple variable references', () => {
    expect(isSimpleReference('items')).toBe(true);
    expect(isSimpleReference('state.list')).toBe(true);
    expect(isSimpleReference('a + b')).toBe(false);
    expect(isSimpleReference('[1, 2]')).toBe(false);
  });

  it('flags multi-line / long values as complex', () => {
    expect(isComplexExpression('a\nb')).toBe(true);
    expect(isComplexExpression('x'.repeat(200))).toBe(true);
    expect(isComplexExpression('a + b')).toBe(false);
  });
});

describe('comprehension clause edits', () => {
  it('seeds a fresh clause by kind', () => {
    expect(makeClause('for')).toEqual({ kind: 'for', binding: 'x', iterable: '[1, 2, 3]' });
    expect(makeClause('if')).toEqual({ kind: 'if', condition: 'true' });
  });

  it('adds a clause immutably', () => {
    const base = [{ kind: 'for', binding: 'a', iterable: 'xs' }];
    const next = clauseAdded(base, 'if');
    expect(next).toHaveLength(2);
    expect(next[1]).toEqual({ kind: 'if', condition: 'true' });
    expect(base).toHaveLength(1); // original untouched
  });

  it('adds a clause to an empty/undefined list', () => {
    expect(clauseAdded(undefined, 'for')).toHaveLength(1);
  });

  it('removes a clause by index immutably', () => {
    const base = [
      { kind: 'for', binding: 'a', iterable: 'xs' },
      { kind: 'if', condition: 'a > 0' },
    ];
    const next = clauseRemoved(base, 0);
    expect(next).toEqual([{ kind: 'if', condition: 'a > 0' }]);
    expect(base).toHaveLength(2);
  });
});
