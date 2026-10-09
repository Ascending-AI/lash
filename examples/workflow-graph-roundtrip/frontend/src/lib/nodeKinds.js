// Visual identity for each WorkflowDocument node kind. One signature hue per
// kind so the canvas reads as a legend at a glance. `glyph` is a short mark
// shown in the node's kind badge.
export const NODE_KINDS = {
  process: { label: 'process', hue: 188, glyph: '⬡', accent: '#38e0d0' },
  call: { label: 'call', hue: 44, glyph: '⚡', accent: '#f5c451' },
  effect: { label: 'effect', hue: 268, glyph: '◆', accent: '#b18cff' },
  data: { label: 'data', hue: 152, glyph: '≋', accent: '#5fd08a' },
  computation: { label: 'computation', hue: 72, glyph: 'ƒ', accent: '#cdd94a' },
  state_update: { label: 'state', hue: 312, glyph: '≔', accent: '#e77ec8' },
  container: { label: 'branch / loop / try', hue: 210, glyph: '⋔', accent: '#6ab0ff' },
  throw: { label: 'throw', hue: 8, glyph: '↯', accent: '#ff8f6b' },
  terminal: { label: 'terminal', hue: 340, glyph: '◉', accent: '#ff6b9d' },
};

export function kindMeta(kind) {
  return NODE_KINDS[kind] ?? { label: kind, hue: 220, glyph: '•', accent: '#8aa0c0' };
}

// A container node (data.kind === 'container') carries an explicit `subkind`
// once the backend projects/round-trips it (and the host sets it on freshly
// added containers). When present it is authoritative; otherwise we recover it
// from the child slots + derived title (older projections, defensive path).
export function containerSubkind(node) {
  const explicit = node?.data?.subkind;
  if (explicit) return explicit;
  const slots = (node?.data?.children ?? []).map((g) => g.slot);
  if (slots.includes('then') || slots.includes('else')) return 'if';
  if (slots.includes('element')) return 'comprehension';
  if (slots.includes('catch') || slots.includes('finally')) return 'try';
  const title = (node?.data?.title ?? '').trim();
  if (/^for\b/.test(title)) return 'for';
  if (/^while\b/.test(title)) return 'while';
  return 'loop';
}

// Badge glyph + label per container sub-kind (all share the container accent so
// while renders exactly like the existing for / if containers).
export const CONTAINER_SUBKINDS = {
  if: { label: 'if', glyph: '⋔' },
  for: { label: 'for', glyph: '↻' },
  while: { label: 'while', glyph: '⟳' },
  try: { label: 'try', glyph: '⛨' },
  scope: { label: 'block', glyph: '{}' },
  comprehension: { label: 'comprehension', glyph: '⟦⟧' },
  loop: { label: 'loop', glyph: '↻' },
};

// Effect verbs that render as a "waiting"-capable node (sleeps / process waits).
export const WAITING_EFFECTS = new Set(['sleep_for', 'await_join']);

// The "+ Add node" palette and per-node operation labels are fed by the
// operation catalog (lib/operations.js) served from GET /operations — the
// single data home for what can be inserted and how each operation is named.
