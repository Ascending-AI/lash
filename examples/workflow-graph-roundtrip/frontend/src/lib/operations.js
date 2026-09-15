// The operation catalog is host-owned data: the backend's `GET /operations` is
// the single source of truth for what a palette can insert and how each node's
// typed fields are shaped (ADR 0026 stance — no client-side catalog to drift).
// If the endpoint is unreachable the palette shows an error/empty state rather
// than falling back to a duplicated hard-coded list.
//
// A catalog entry is
//   { id, label, nodeKind, subkind?, operation?, effect?, terminalKind?,
//     fields: [{ name, type, default }] }
// where `type` is string | number | boolean | expression | identifier |
// assignment_target.

import { NODE_KINDS, CONTAINER_SUBKINDS, kindMeta } from './nodeKinds.js';

// Palette groups, in menu order. Each entry lands in the first group whose
// `kinds` include its nodeKind; unmatched kinds fall into "Other".
const GROUPS = [
  { id: 'actions', label: 'Actions', kinds: ['call', 'effect'] },
  { id: 'control', label: 'Control flow', kinds: ['container', 'terminal'] },
  { id: 'data', label: 'Data', kinds: ['data', 'computation', 'state_update'] },
  { id: 'process', label: 'Process', kinds: ['process'] },
  { id: 'advanced', label: 'Advanced', kinds: ['opaque'] },
];

// Kinds hidden from the Simplified palette (raw escape hatches).
const POWER_KINDS = new Set(['opaque']);

// Kinds that may only be inserted at the top level, not into a container slot.
const TOP_LEVEL_KINDS = new Set(['process']);

// Group a catalog into `[{ id, label, items }]`, dropping empty groups.
// Returns `[]` for a missing/empty catalog so the palette can show an empty
// state. Simplified mode hides raw power-only kinds; `topLevel:false` (a
// container's add-menu) additionally hides top-level-only kinds like `process`.
export function groupOperations(catalog, { includePower = true, topLevel = true } = {}) {
  const entries = (catalog ?? []).filter(
    (op) =>
      (includePower || !POWER_KINDS.has(op.nodeKind)) && (topLevel || !TOP_LEVEL_KINDS.has(op.nodeKind)),
  );
  if (!entries.length) return [];
  const groups = GROUPS.map((g) => ({
    id: g.id,
    label: g.label,
    items: entries.filter((op) => g.kinds.includes(op.nodeKind)),
  }));
  const claimed = new Set(GROUPS.flatMap((g) => g.kinds));
  const other = entries.filter((op) => !claimed.has(op.nodeKind));
  if (other.length) groups.push({ id: 'other', label: 'Other', items: other });
  return groups.filter((g) => g.items.length);
}

// The catalog entries a node of `nodeKind` can switch between (its operation
// options) — display calls for `call`, sleep/wait_signal for `effect`, etc.
export function operationsForKind(catalog, nodeKind) {
  return (catalog ?? []).filter((op) => op.nodeKind === nodeKind);
}

// The raw source text of a catalog field's default.
//
// The catalog serves an expression-valued default as `{ "$expr": "<source>" }`
// (see `EditableValue::Expr` on the wire), so `String(default)` on one yields
// `[object Object]` — the literal that used to reach both the seeded arg form
// and the synthesized receiver call (FIG-3178). Everything else is already its
// own slot text.
export function defaultSource(value) {
  if (value && typeof value === 'object' && typeof value.$expr === 'string') return value.$expr;
  return String(value ?? '');
}

// A single field's default as an EditableValue for the `data.fields` map.
export function fieldDefaultValue(field) {
  switch (field.type) {
    case 'number':
      return Number(field.default ?? 0) || 0;
    case 'boolean':
      return !!field.default;
    case 'expression':
      return { $expr: defaultSource(field.default) };
    default:
      return defaultSource(field.default);
  }
}

// The seed `data.fields` map for an operation entry (used to prefill the arg
// form when a node is inserted or switched to a different operation).
export function catalogFieldsMap(op) {
  const out = {};
  for (const field of op.fields ?? []) out[field.name] = fieldDefaultValue(field);
  return out;
}

// One `name: value` record argument for a synthesized receiver call.
function recordArg(field) {
  const value = field.default;
  switch (field.type) {
    case 'number':
      return `${field.name}: ${Number(value ?? 0) || 0}`;
    case 'boolean':
      return `${field.name}: ${value ? 'true' : 'false'}`;
    case 'string':
      return `${field.name}: ${JSON.stringify(String(value ?? ''))}`;
    default:
      return `${field.name}: ${defaultSource(value)}`; // expression / identifier — raw
  }
}

// A call node's expression is the awaited receiver call the lens projects back
// out of source. The `await` is load-bearing, not cosmetic: an unawaited tool
// call lowers to a pending-tool value rather than a receiver call, so the
// backend cannot resolve its operation and refuses the save with
// `invalid_expression` (FIG-3177). This is the string the backend also
// synthesizes for itself when a call node arrives with no expression.
export function synthCallExpression(op) {
  const args = (op.fields ?? []).map(recordArg).join(', ');
  // The receiver comes from the catalog entry, not from the operation name:
  // `list_recent` belongs to `gmail`, and synthesizing `display.list_recent`
  // gives the lowerer a receiver that has no such operation (FIG-3178).
  return `await ${op.receiver ?? 'display'}.${op.operation}({ ${args} })`;
}

// The `data` patch to apply when switching an existing call/effect to a
// different operation: point at the new operation and refill the arg form from
// the new operation's typed defaults.
//
// A call carries its receiver too, and its expression is re-synthesized from
// the chosen entry rather than method-renamed: the operation name alone does
// not name a receiver, so switching `display.show_message` to `gmail.list_recent`
// by swapping the method leaves `display.list_recent`, which no receiver serves
// (FIG-3179). Effects rebuild from `effect` + fields, so they drop any stored
// `expression` instead.
export function operationSwitchPatch(nodeKind, op) {
  const patch = { fields: catalogFieldsMap(op) };
  if (nodeKind === 'call') {
    patch.operation = op.operation;
    patch.receiver = op.receiver ?? null;
    patch.expression = synthCallExpression(op);
  } else if (nodeKind === 'effect') {
    patch.effect = op.effect;
    patch.clearExpression = true;
  }
  return patch;
}

// The catalog entry a node currently matches, so an operation `<select>` can
// preselect it: calls key on `operation`, effects on `effect`, terminals on
// `terminalKind`.
export function currentOperationId(catalog, node) {
  const kind = node?.data?.kind;
  const match = (predicate) => (catalog ?? []).find(predicate)?.id ?? null;
  if (kind === 'call') return match((op) => op.operation === node.data.operation);
  if (kind === 'effect') return match((op) => op.effect === node.data.effect);
  if (kind === 'terminal') return match((op) => op.terminalKind === node.data.terminalKind);
  return null;
}

// Menu glyph + accent for a catalog entry (containers borrow the container
// accent + their sub-kind glyph, so the palette reads like the canvas legend).
export function operationMeta(op) {
  if (op.nodeKind === 'container' && op.subkind) {
    const sub = CONTAINER_SUBKINDS[op.subkind] ?? CONTAINER_SUBKINDS.loop;
    return { glyph: sub.glyph, accent: NODE_KINDS.container.accent };
  }
  const meta = NODE_KINDS[op.nodeKind] ?? kindMeta(op.nodeKind);
  return { glyph: meta.glyph, accent: meta.accent };
}
