// Plain-language STEP-LIST model (FIG-391, iteration 1).
//
// A non-technical person reads a workflow as a linear checklist, not a node
// canvas. This module turns a `WorkflowDocument` (the same shape the canvas
// consumes — see CONTRACT.md and lib/graph.js) into an ordered, nested tree of
// plain-language "steps", and maps each node kind to a human label. It is PURE
// (no Svelte, no DOM) so the label mapping, the token-vs-static decision, the
// nesting builder, and the diagnostic→row mapping are all unit-testable.
//
// It NEVER re-parses Lashlang: value classification consumes the facets the
// backend already attached (availableVars / expectedArgTypes), exactly like
// lib/facets.js. Vocabulary here is deliberately soft ("Save X as", "Only if")
// and will iterate.

import { normalizeVars } from './facets.js';
import { parseLiteral, isSimpleReference } from './fields.js';

// --- nesting builder -------------------------------------------------------

// Build the ordered, nested step tree from a document.
// Node order IS the document's source order (roots + each group's `nodeIds`), so no layout is
// needed — we just walk it.
// A container/process entry carries `groups`, one per child slot, each with a plain-language
// `header` (null when the steps sit directly under the container's own label, e.g. an `if`'s
// `then`) and its own nested `steps`.
export function buildSteps(doc) {
  const nodeMap = new Map((doc?.nodes ?? []).map((n) => [n.id, n]));
  const inputsByTarget = new Map();
  for (const edge of doc?.edges ?? []) {
    if (edge.data?.kind !== 'data' || !edge.data.variable) continue;
    const inputs = inputsByTarget.get(edge.target) ?? [];
    if (!inputs.includes(edge.data.variable)) inputs.push(edge.data.variable);
    inputsByTarget.set(edge.target, inputs);
  }

  function build(id, depth, seen) {
    const node = nodeMap.get(id);
    if (!node || seen.has(id)) return null;
    seen.add(id);
    const kind = node.data?.kind;
    const subkind = kind === 'container' ? containerSubkindOf(node) : null;
    const groups = [];
    for (const grp of node.data?.children ?? []) {
      const steps = grp.nodeIds
        .map((cid) => build(cid, depth + 1, seen))
        .filter(Boolean);
      groups.push({
        slot: grp.slot,
        header: groupHeader(subkind, grp.slot),
        steps,
      });
    }
    const vars = normalizeVars(node.data?.availableVars ?? []);
    const inputs = (inputsByTarget.get(id) ?? []).map((name) => ({
      name,
      type: vars.find((variable) => variable.name === name)?.type ?? 'any',
    }));
    return { id, node, kind, subkind, depth, groups, inputs };
  }

  const seen = new Set();
  return {
    processes: (doc?.roots?.processes ?? [])
      .map((id) => build(id, 0, seen))
      .filter(Boolean),
    main: (doc?.roots?.main ?? []).map((id) => build(id, 0, seen)).filter(Boolean),
  };
}

// A workflow that is a single process with no top-level statements reads best as a flat
// Trigger→ Action stack: we surface that lone process's body directly as the primary flow (no
// "Background tasks" wrapper, no process card — its @label title is already shown in the
// workflow selector, so we never repeat it as a card).
// Any other shape (multiple processes, or a top-level `main` alongside one or more processes)
// keeps the grouped presentation.
// PURE: consumes buildSteps, chooses a layout, and — when flattened — carries the owning
// process's insert target so the rail "+" lands inside its body.
export function presentSteps(doc) {
  const tree = buildSteps(doc);
  if (tree.processes.length === 1 && tree.main.length === 0) {
    const proc = tree.processes[0];
    const body = (proc.groups ?? []).find((g) => g.slot === 'body') ?? proc.groups?.[0] ?? null;
    return {
      flat: true,
      flowName: proc.node?.data?.title ?? proc.node?.data?.name ?? null,
      steps: body?.steps ?? [],
      insertTarget: body ? { ownerId: proc.id, slot: body.slot } : null,
      processes: [],
      main: [],
    };
  }
  return {
    flat: false,
    flowName: null,
    steps: [],
    insertTarget: null,
    processes: tree.processes,
    main: tree.main,
  };
}

// The tag a step carries in a primary flow. The FIRST step of a primary flow is
// the unmistakable Trigger (what starts the workflow); the rest are numbered
// Actions. A step nested inside a branch/loop is not primary — it reads as a
// plain numbered Step within that block. Pure so the trigger-vs-action rule is
// unit-testable; the renderer styles the `role` and shows the source name.
export function stepTag(primary, index) {
  if (primary && index === 0) return { role: 'trigger', text: 'Trigger' };
  if (primary) return { role: 'action', text: `Action ${index + 1}` };
  return { role: 'step', text: `Step ${index + 1}` };
}

// The container sub-kind, mirroring nodeKinds.js#containerSubkind but without a
// Svelte import (kept local so steps.js stays framework-free and testable).
export function containerSubkindOf(node) {
  const explicit = node?.data?.subkind;
  if (explicit) return explicit;
  const slots = (node?.data?.children ?? []).map((g) => g.slot);
  if (slots.includes('then') || slots.includes('else')) return 'if';
  if (slots.includes('element')) return 'comprehension';
  const title = (node?.data?.title ?? '').trim();
  if (/^for\b/.test(title)) return 'for';
  if (/^while\b/.test(title)) return 'while';
  return 'loop';
}

// A sub-header for a child group, or null when the group's steps read directly
// under the container's own label. Only the `if`'s `else` arm needs one
// ("Otherwise"). Everything else (a `then`, a loop `body`, a process `body`, a
// comprehension `element`) flows straight under the parent line.
export function groupHeader(subkind, slot) {
  if (subkind === 'if' && slot === 'else') return 'Otherwise';
  return null;
}

// --- plain-language labels -------------------------------------------------

// NEVER leaks "expression/SSA/state_update/comprehension/await".
// Display side-effects that are UI feedback, not user-meaningful workflow steps.
// They stay visible (nothing is hidden) but render muted so they don't compete with real
// actions in the checklist.
// `show_message` is NOT here — it's the user-facing output and stays prominent.
const DISPLAY_FEEDBACK = new Set(['set_status', 'set_progress', 'set_light', 'highlight']);

export function stepLabel(node, catalog = []) {
  const d = node?.data ?? {};
  const kind = d.kind;

  switch (kind) {
    case 'call': {
      const minor = DISPLAY_FEEDBACK.has(d.operation ?? '');
      const operation = operationCatalogEntry(node, catalog);
      const icon = toolIconKey(operation?.id ?? d.operation);
      return {
        category: 'action',
        glyph: minor ? '·' : null,
        lead: 'Do',
        name: actionTitle(node, catalog),
        icon,
        toolName: toolName(icon),
        operationId: operation?.id ?? d.operation ?? '',
        minor,
      };
    }
    case 'data':
    case 'computation':
      return { category: 'save', glyph: '≋', lead: 'Save', name: bindingName(d) };
    case 'state_update':
      return { category: 'set', glyph: '≔', lead: 'Set', name: targetName(d) };
    case 'terminal':
      return d.terminalKind === 'fail'
        ? { category: 'fail', glyph: '◉', lead: 'Stop with an error', name: null }
        : { category: 'finish', glyph: '◉', lead: 'Finish with', name: null };
    case 'process':
      return {
        category: 'process',
        glyph: '⬡',
        lead: 'Background task',
        name: d.name ?? d.title ?? 'task',
      };
    case 'opaque':
      return { category: 'opaque', glyph: '{}', lead: 'Advanced step', name: d.title ?? null };
    case 'effect':
      return effectLabel(node);
    case 'container':
      return containerLabel(node);
    default:
      return { category: 'unknown', glyph: '•', lead: d.title ?? kind ?? 'Step', name: null };
  }
}

function containerLabel(node) {
  switch (containerSubkindOf(node)) {
    case 'if':
      return { category: 'if', glyph: '⋔', lead: 'Only if', name: null };
    case 'while':
      return { category: 'while', glyph: '⟳', lead: 'Keep going while', name: null };
    case 'for':
      return {
        category: 'for',
        glyph: '↻',
        lead: 'For each',
        name: node.data?.binding ?? 'item',
      };
    case 'comprehension':
      return { category: 'comprehension', glyph: '⟦⟧', lead: 'Make a list', name: null };
    default:
      return { category: 'loop', glyph: '↻', lead: 'Repeat', name: null };
  }
}

function effectLabel(node) {
  const d = node?.data ?? {};
  switch (d.effect) {
    case 'start_process':
      return { category: 'start', glyph: '▷', lead: 'Start', name: effectSubject(d), tail: 'in the background' };
    case 'await_join':
      return { category: 'await', glyph: '⏳', lead: 'Wait for', name: effectSubject(d), tail: 'to finish' };
    case 'wait_signal':
      return { category: 'waitSignal', glyph: '⏳', lead: 'Wait for', name: signalName(d) };
    case 'sleep':
      return { category: 'sleep', glyph: '⏸', lead: 'Wait for', name: durationName(d) };
    case 'signal_run':
      return { category: 'effect', glyph: '◆', lead: 'Notify', name: effectSubject(d) };
    case 'cancel':
      return { category: 'effect', glyph: '◆', lead: 'Cancel', name: effectSubject(d) };
    default:
      return { category: 'effect', glyph: '◆', lead: d.title ?? 'Do', name: null };
  }
}

// A direct id match wins.
// The graph contract otherwise stores the receiver operation's final segment, so duplicate
// names such as Slack's and GitHub's `recent` are disambiguated by the host-owned argument
// schema (`channel` versus `repo`).
// No source parsing.
export function operationCatalogEntry(node, catalog = []) {
  const d = node?.data ?? {};
  const operation = String(d.operation ?? '');
  if (!operation) return null;
  const entries = catalog ?? [];

  const direct = entries.find((entry) => entry?.id === operation);
  if (direct) return direct;

  const candidates = entries.filter((entry) => entry?.operation === operation);
  if (candidates.length <= 1) return candidates[0] ?? null;

  const nodeFields = new Set(Object.keys(d.fields ?? {}));
  const ranked = candidates
    .map((entry) => {
      const catalogFields = new Set((entry.fields ?? []).map((field) => field.name));
      let score = 0;
      for (const name of nodeFields) score += catalogFields.has(name) ? 2 : -1;
      for (const name of catalogFields) if (!nodeFields.has(name)) score -= 1;
      return { entry, score };
    })
    .sort((a, b) => b.score - a.score);
  return ranked[0].score > ranked[1].score ? ranked[0].entry : null;
}

function actionTitle(node, catalog) {
  const d = node?.data ?? {};
  const t = (d.title ?? '').trim();
  if (d.nameSource === 'label' && t) return t;
  const catalogLabel = (operationCatalogEntry(node, catalog)?.label ?? '').trim();
  if (catalogLabel) return catalogLabel;
  if (d.operation) return humanizeIdent(d.operation);
  if (t) return t;
  return 'this action';
}

// A stable local icon vocabulary. The key can be a full catalog id
// (`slack.recent`) or a graph operation fallback (`show_message`).
export function toolIconKey(identifier) {
  const id = String(identifier ?? '').trim().toLowerCase();
  const namespace = id.split(/[.:/]/)[0];
  if (namespace === 'slack') return 'slack';
  if (namespace === 'github') return 'github';
  if (namespace === 'gmail' || namespace === 'email' || namespace === 'mail') return 'email';
  if (namespace === 'web' || id === 'search_web' || id.includes('web_search')) return 'web';
  if (namespace === 'llm' || id === 'llm_query') return 'llm';
  if (namespace === 'agents' || id === 'spawn_agent' || id.includes('agent.spawn')) return 'agent';
  if (id === 'show_message' || id.endsWith('.show_message')) return 'message';
  return 'action';
}

function toolName(icon) {
  switch (icon) {
    case 'slack':
      return 'Slack';
    case 'github':
      return 'GitHub';
    case 'email':
      return 'Email';
    case 'web':
      return 'Web';
    case 'llm':
      return 'AI';
    case 'agent':
      return 'Agent';
    case 'message':
      return 'Display';
    default:
      return 'Action';
  }
}

function bindingName(d) {
  const b = (d.binding ?? '').trim();
  return b || 'this value';
}
function targetName(d) {
  const t = (d.target ?? '').trim();
  return t || 'this value';
}
function effectSubject(d) {
  const f = d.fields ?? {};
  return String(f.name ?? f.process ?? f.handle ?? d.title ?? 'task');
}
function signalName(d) {
  const f = d.fields ?? {};
  return String(f.signal ?? d.title ?? 'a signal');
}
function durationName(d) {
  const f = d.fields ?? {};
  return f.duration != null ? String(f.duration) : 'a moment';
}

// `show_message` → "Show message". A gentle touch so a derived operation id
// reads as words; author-set titles are used verbatim (see actionTitle).
export function humanizeIdent(ident) {
  return String(ident ?? '')
    .replace(/[_.]+/g, ' ')
    .replace(/\s+/g, ' ')
    .trim()
    .replace(/^./, (c) => c.toUpperCase());
}

// --- value classification (token vs typed-in) ------------------------------

// A value that is exactly an in-scope variable (a value produced by an earlier step) becomes a
// colored TOKEN chip carrying its type; a bare literal becomes plain STATIC text (humanized);
// anything compound stays an EXPRESSION pill.
// Consumes the facet `availableVars` — never re-parses Lashlang beyond the trivial literal
// shapes lib/fields.js already recognizes.
export function describeValue(text, availableVars = []) {
  const raw = text ?? '';
  const trimmed = raw.trim();
  const vars = normalizeVars(availableVars);
  if (trimmed === '') return { kind: 'static', text: '', literalType: 'string', display: '' };

  const hit = vars.find((v) => v.name === trimmed);
  if (hit && isSimpleReference(trimmed)) {
    return { kind: 'token', name: hit.name, varType: hit.type ?? 'any' };
  }

  const lit = parseLiteral(trimmed);
  if (lit.type !== 'expression') {
    return {
      kind: 'static',
      text: trimmed,
      literalType: lit.type,
      display: humanizeLiteral(lit),
    };
  }
  return { kind: 'expression', text: trimmed };
}

// Human display for a scalar literal: a quoted string shows its bare contents,
// a boolean reads as Yes/No, a number is itself.
export function humanizeLiteral(lit) {
  switch (lit.type) {
    case 'string':
      return lit.value === '' ? '(empty)' : lit.value;
    case 'boolean':
      return lit.value === 'true' ? 'Yes' : 'No';
    default:
      return String(lit.value);
  }
}

// --- collapsed-card summary ------------------------------------------------

// A one-line, plain-language gist of a step's key configuration, shown on a COLLAPSED
// action/effect card so a reader understands what it does without expanding it (the main
// density fix).
// Consumes the node's typed `data.fields` (host-owned) and NEVER re-parses Lashlang — an
// expression-valued arg is shown as its stored text.
// Caps at the first three set fields so a wide action stays a single calm line.
export function stepSummary(node) {
  const d = node?.data ?? {};
  if (d.kind !== 'call' && d.kind !== 'effect') return '';
  const parts = [];
  for (const value of Object.values(d.fields ?? {})) {
    const text = summarizeFieldValue(value);
    if (text) parts.push(text);
    if (parts.length >= 3) break;
  }
  return parts.join(' · ');
}

// Plain text for one field value in a collapsed summary: a stored expression
// shows its text, a boolean reads Yes/No, a number is itself, an empty value is
// skipped (returns '').
export function summarizeFieldValue(value) {
  if (value == null) return '';
  if (typeof value === 'boolean') return value ? 'Yes' : 'No';
  if (typeof value === 'number') return String(value);
  if (typeof value === 'object' && typeof value.$expr === 'string') return value.$expr.trim();
  return String(value).trim();
}

// --- friendly diagnostics --------------------------------------------------

// Turn a facet diagnostic into a plain sentence for a row. Known type-error
// kinds get a soft, non-technical rewrite; anything else falls back to the
// backend's own message (still surfaced, never swallowed).
export function friendlyDiagnostic(diag) {
  switch (diag?.kind) {
    case 'incompatible_iteration_target':
      return 'This step goes through a list, but this value is not a list.';
    case 'incompatible_process_return':
      return 'This background task hands back a different kind of value than expected.';
    case 'incompatible_argument_type':
    case 'type_mismatch':
      return diag.message || 'This value is the wrong kind for this step.';
    default:
      return diag?.message || 'Something about this step does not fit.';
  }
}

// Every node's diagnostics as friendly row sentences: `[{ kind, message,
// friendly }]`. Empty when the node is clean.
export function rowDiagnostics(node) {
  return (node?.data?.diagnostics ?? []).map((d) => ({
    kind: d.kind,
    message: d.message,
    friendly: friendlyDiagnostic(d),
  }));
}
