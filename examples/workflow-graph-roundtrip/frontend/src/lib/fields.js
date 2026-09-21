// Helpers shared by the typed field forms.

// The `/validate` kind for an editable text slot. Expression-valued slots
// (conditions, iterables, computed values, `$expr` record args) validate as
// `expression`; assignment targets as `assignment_target`; simple `let`/loop
// bindings as `identifier`.
export function validationKind(slot) {
  switch (slot) {
    case 'target':
      return 'assignment_target';
    case 'binding':
    case 'clauseBinding':
      return 'identifier';
    default:
      return 'expression';
  }
}

// A value is "too complex" for the visual builders (multi-line or long); the
// field then stays on the raw-text editor rather than a builder widget.
export function isComplexExpression(text) {
  if (typeof text !== 'string') return false;
  return text.includes('\n') || text.length > 96;
}

// Comparison operators for the if/while condition builder, in display order.
export const COMPARISON_OPS = [
  { value: '<', label: '<' },
  { value: '<=', label: '≤' },
  { value: '==', label: '=' },
  { value: '!=', label: '≠' },
  { value: '>=', label: '≥' },
  { value: '>', label: '>' },
];

const OP_ALT = new Set(COMPARISON_OPS.map((o) => o.value));

// True when every bracket/paren in `s` is balanced (ignoring string contents).
// Used to reject comparison operands that carry a stray opening/closing paren —
// e.g. the `(x` / `2)` fragments a naive split of `(x < 2)` would produce.
function bracketsBalanced(s) {
  let depth = 0;
  let inStr = false;
  let esc = false;
  for (const ch of s) {
    if (inStr) {
      if (esc) esc = false;
      else if (ch === '\\') esc = true;
      else if (ch === '"') inStr = false;
      continue;
    }
    if (ch === '"') inStr = true;
    else if (ch === '(' || ch === '[' || ch === '{') depth += 1;
    else if (ch === ')' || ch === ']' || ch === '}') {
      depth -= 1;
      if (depth < 0) return false;
    }
  }
  return depth === 0 && !inStr;
}

// Strip a balanced outer paren pair that wraps the WHOLE expression, repeatedly.
// The lens emits every `if`/`while` condition parenthesized (`(x < 2)`,
// `(state.count < 3)`), so a loaded/saved condition must be unwrapped before the
// comparison builder can read it. Only strips when the leading `(` closes at the
// final char — `(a) < (b)` is left intact.
export function stripOuterParens(text) {
  let t = (text ?? '').trim();
  // Guard against pathological input.
  for (let guard = 0; guard < 64; guard += 1) {
    if (!(t.startsWith('(') && t.endsWith(')'))) break;
    let depth = 0;
    let inStr = false;
    let esc = false;
    let wrapsWhole = true;
    for (let i = 0; i < t.length; i += 1) {
      const ch = t[i];
      if (inStr) {
        if (esc) esc = false;
        else if (ch === '\\') esc = true;
        else if (ch === '"') inStr = false;
        continue;
      }
      if (ch === '"') inStr = true;
      else if (ch === '(') depth += 1;
      else if (ch === ')') {
        depth -= 1;
        // The opening paren closed before the end → it does not wrap the whole.
        if (depth === 0 && i !== t.length - 1) {
          wrapsWhole = false;
          break;
        }
      }
    }
    if (!wrapsWhole || depth !== 0) break;
    t = t.slice(1, -1).trim();
  }
  return t;
}

// The condition is unwrapped first so a lens-emitted `(x < 2)` reads as lhs=`x`, rhs=`2`
// rather than the bogus `(x` / `2)` a naive split would yield.
export function parseComparison(text) {
  const source = stripOuterParens(text ?? '');
  const m = /^\s*([^<>=!]+?)\s*(<=|>=|==|!=|<|>)\s*(.+?)\s*$/.exec(source);
  if (!m) return null;
  // Reject a right side that itself contains a comparison operator (chained /
  // compound expressions belong in the raw editor).
  if (/[<>]|[=!]=/.test(m[3])) return null;
  if (!OP_ALT.has(m[2])) return null;
  // Reject operands with unbalanced brackets — that means the split cut through
  // a parenthesized/compound expression (e.g. `(x < 2) && y`), which is raw.
  if (!bracketsBalanced(m[1]) || !bracketsBalanced(m[3])) return null;
  return { lhs: m[1].trim(), op: m[2], rhs: m[3].trim() };
}

// Classify a value literal for the value builder. `expression` means "not a
// simple literal" — such values stay on the raw editor.
export function parseLiteral(text) {
  const t = (text ?? '').trim();
  if (t === '') return { type: 'string', value: '' };
  if (/^-?\d+(\.\d+)?$/.test(t)) return { type: 'number', value: t };
  if (t === 'true' || t === 'false') return { type: 'boolean', value: t };
  const s = /^"((?:[^"\\]|\\.)*)"$/.exec(t);
  if (s) return { type: 'string', value: s[1] };
  return { type: 'expression', value: t };
}

// Encode a builder value back into canonical Lashlang literal text.
export function encodeLiteral(type, value) {
  switch (type) {
    case 'number': {
      const n = String(value ?? '').trim();
      return n === '' ? '0' : n;
    }
    case 'boolean':
      return value === true || value === 'true' ? 'true' : 'false';
    case 'string':
      return JSON.stringify(String(value ?? ''));
    default:
      return String(value ?? '');
  }
}

// A bare boolean literal — a freshly-seeded if/while condition — which the
// comparison builder treats as "start a fresh comparison" rather than raw.
export function isBoolLiteral(text) {
  const t = (text ?? '').trim();
  return t === 'true' || t === 'false';
}

// A simple variable reference (`items`, `state.list`) — used by the list builder
// to offer "iterate an in-scope variable" instead of a literal list.
export function isSimpleReference(text) {
  return /^[A-Za-z_]\w*(\.[A-Za-z_]\w*)*$/.test((text ?? '').trim());
}

// Classify an operand for the scalar/variable builder (comparison operands,
// value fields, list items). Returns `'var'` when the value is an in-scope
// variable, a scalar literal kind (`number`/`string`/`boolean`), or
// `'expression'` for a compound value that must stay on the raw editor.
export function operandType(value, vars = []) {
  const trimmed = (value ?? '').trim();
  if (Array.isArray(vars) && vars.length && isSimpleReference(value) && vars.includes(trimmed)) {
    return 'var';
  }
  return parseLiteral(value).type;
}

function splitTopLevel(source) {
  const out = [];
  let depth = 0;
  let inStr = false;
  let esc = false;
  let cur = '';
  for (const ch of source) {
    if (inStr) {
      cur += ch;
      if (esc) esc = false;
      else if (ch === '\\') esc = true;
      else if (ch === '"') inStr = false;
      continue;
    }
    if (ch === '"') {
      inStr = true;
      cur += ch;
    } else if (ch === '[' || ch === '(' || ch === '{') {
      depth += 1;
      cur += ch;
    } else if (ch === ']' || ch === ')' || ch === '}') {
      depth -= 1;
      cur += ch;
    } else if (ch === ',' && depth === 0) {
      out.push(cur);
      cur = '';
    } else {
      cur += ch;
    }
  }
  if (inStr || depth !== 0) return null;
  out.push(cur);
  return out;
}

export function parseList(text) {
  const t = (text ?? '').trim();
  if (!t.startsWith('[') || !t.endsWith(']')) return null;
  const inner = t.slice(1, -1).trim();
  if (inner === '') return { items: [] };
  const parts = splitTopLevel(inner);
  if (parts === null) return null;
  const items = [];
  for (const part of parts) {
    const lit = parseLiteral(part.trim());
    if (lit.type === 'expression') return null;
    items.push(lit);
  }
  return { items };
}

// Encode scalar items back into a canonical Lashlang list literal.
export function encodeList(items) {
  return `[${(items ?? []).map((it) => encodeLiteral(it.type, it.value)).join(', ')}]`;
}

// A fresh comprehension clause of the given kind, seeded sensibly.
export function makeClause(kind) {
  return kind === 'for'
    ? { kind: 'for', binding: 'x', iterable: '[1, 2, 3]' }
    : { kind: 'if', condition: 'true' };
}

// Immutable clause list edits (extracted so they can be unit-tested).
export function clauseAdded(clauses, kind) {
  return [...(clauses ?? []), makeClause(kind)];
}
export function clauseRemoved(clauses, index) {
  return (clauses ?? []).filter((_, i) => i !== index);
}
