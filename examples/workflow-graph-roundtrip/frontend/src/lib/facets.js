// Consume the derived, read-only TYPE FACETS the backend attaches to each node
// (ADR 0073). These are host-supplied and non-authoritative: we NEVER re-parse
// Lashlang client-side — we only read the facet shapes the lens already emitted.
//
// Serialized shapes (see examples/workflow-graph-roundtrip/src/contract.rs):
//   node.data.availableVars     [{ name, type }]        (was names-only)
//   node.data.expectedArgTypes  [{ slot, type }]        slot = "arg[0]", "arg[0].field", "arg[0][2]", "call[1].arg[0]"
//   node.data.diagnostics       [{ nodeId, kind, message, span? }]
//   document.facetSchemaVersion number | undefined      (absent on older backends)
//
// Type strings come from lashlang's `format_type_expr`:
//   any · str · int · float · bool · dict · null
//   enum["a", "b"] · list[T] · { f: t, g: t? } · <RefName> · Process<in, out>
//   TriggerHandle<E> · unions as `a | b | null`
// NOTE the lowercase, bracketed spelling (`list[...]`, `enum[...]`, `any`,
// `dict`) — the compatibility logic keys off these exact tokens.

// True when the document carries the derived facet contract at all. Older
// backends omit `facetSchemaVersion`; the whole editor experience degrades to
// its pre-facet behaviour when this is false (no filtering, no blocking).
export function hasFacets(doc) {
  return doc != null && doc.facetSchemaVersion != null;
}

// --- type parsing ----------------------------------------------------------

// Split `s` on top-level occurrences of `sep` (a single char), respecting
// brackets `[] {} <> ()` and double-quoted strings. Returns the raw fragments.
function splitTopLevel(s, sep) {
  const out = [];
  let depth = 0;
  let inStr = false;
  let esc = false;
  let cur = '';
  for (const ch of s) {
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
    } else if (ch === '[' || ch === '{' || ch === '<' || ch === '(') {
      depth += 1;
      cur += ch;
    } else if (ch === ']' || ch === '}' || ch === '>' || ch === ')') {
      depth -= 1;
      cur += ch;
    } else if (ch === sep && depth === 0) {
      out.push(cur);
      cur = '';
    } else {
      cur += ch;
    }
  }
  out.push(cur);
  return out;
}

// Parse a formatted type string into a light descriptor `{ kind, ... }`.
// Unknown / nominal references collapse to `{ kind: 'ref', name }`.
export function parseType(str) {
  const t = (str ?? '').trim();
  if (t === '') return { kind: 'any' };

  const unionParts = splitTopLevel(t, '|');
  if (unionParts.length > 1) {
    return { kind: 'union', members: unionParts.map((p) => parseType(p)) };
  }

  switch (t) {
    case 'any':
      return { kind: 'any' };
    case 'dict':
      return { kind: 'dict' };
    case 'str':
      return { kind: 'str' };
    case 'int':
      return { kind: 'int' };
    case 'float':
      return { kind: 'float' };
    case 'bool':
      return { kind: 'bool' };
    case 'null':
      return { kind: 'null' };
    default:
      break;
  }

  if (t.startsWith('list[') && t.endsWith(']')) {
    return { kind: 'list', item: parseType(t.slice(5, -1)) };
  }
  if (t.startsWith('enum[') && t.endsWith(']')) {
    return { kind: 'enum', members: parseEnumMembers(t.slice(5, -1)) };
  }
  if (t.startsWith('{') && t.endsWith('}')) {
    return { kind: 'object' };
  }
  if (t.startsWith('Process<')) return { kind: 'process' };
  if (t.startsWith('TriggerHandle<')) return { kind: 'trigger' };
  return { kind: 'ref', name: t };
}

// The members of an `enum[...]` type as plain (unquoted) strings, or null when
// the type is not an enum. Used to render a dropdown instead of a free field.
export function enumMembers(str) {
  const parsed = parseType(str);
  return parsed.kind === 'enum' ? parsed.members : null;
}

// The canonical Lashlang text for a chosen enum member (a quoted string).
export function enumMemberToText(member) {
  return JSON.stringify(String(member ?? ''));
}

// Which enum member the current slot text selects, or null when the text is not
// one of the members (a variable / custom expression the user typed raw). Reads
// both a quoted literal (`"ok"`, from an expression slot) and a bare member
// (`ok`, from a plain string field).
export function enumMemberFromText(text, members) {
  const t = (text ?? '').trim();
  const quoted = /^"((?:[^"\\]|\\.)*)"$/.exec(t);
  const candidate = quoted ? quoted[1].replace(/\\"/g, '"').replace(/\\\\/g, '\\') : t;
  return (members ?? []).includes(candidate) ? candidate : null;
}

function parseEnumMembers(inner) {
  const body = inner.trim();
  if (body === '') return [];
  return splitTopLevel(body, ',').map((part) => {
    const p = part.trim();
    const m = /^"((?:[^"\\]|\\.)*)"$/.exec(p);
    return m ? m[1].replace(/\\"/g, '"').replace(/\\\\/g, '\\') : p;
  });
}

// A type that offers/accepts anything under gradual typing: `any` and the open
// `dict`. An empty/absent type is gradual too (nothing known → don't restrict).
function isGradual(node) {
  return node.kind === 'any' || node.kind === 'dict';
}

function isNumeric(node) {
  return node.kind === 'int' || node.kind === 'float';
}

// Is a variable of type `varType` compatible with an argument slot expecting
// `expectedType`? Honors gradual `any`/`dict` on either side, unions, numeric
// int/float interchange, enum⇄str refinement, and structural list nesting.
export function typesCompatible(varType, expectedType) {
  if ((expectedType ?? '').trim() === '') return true;
  return compat(parseType(varType), parseType(expectedType));
}

function compat(v, e) {
  // An `any`/`dict` slot offers everything; an `any`/`dict` variable is always
  // offered — this is what keeps the editor gradual rather than punishing.
  if (isGradual(e) || isGradual(v)) return true;

  if (e.kind === 'union') return e.members.some((m) => compat(v, m));
  if (v.kind === 'union') return v.members.some((m) => compat(m, e));

  if (isNumeric(v) && isNumeric(e)) return true;

  if (e.kind === 'list') return v.kind === 'list' && compat(v.item, e.item);
  if (v.kind === 'list') return false;

  // Enum is a refinement of str: a str-typed var may fill an enum slot and an
  // enum-typed var may fill a str slot.
  if (e.kind === 'enum') return v.kind === 'enum' || v.kind === 'str';
  if (v.kind === 'enum') return e.kind === 'str';

  if (e.kind === 'ref' || v.kind === 'ref') {
    return v.kind === 'ref' && e.kind === 'ref' && v.name === e.name;
  }

  return v.kind === e.kind;
}

// --- variable facets -------------------------------------------------------

// Normalize `availableVars` to `[{ name, type }]`. Tolerates the legacy
// names-only shape (older backend) by typing every entry `any`.
export function normalizeVars(availableVars) {
  if (!Array.isArray(availableVars)) return [];
  return availableVars.map((v) =>
    typeof v === 'string' ? { name: v, type: 'any' } : { name: v?.name ?? '', type: v?.type ?? 'any' },
  );
}

// All in-scope variable NAMES (unfiltered).
export function varNames(availableVars) {
  return normalizeVars(availableVars).map((v) => v.name);
}

// The names of in-scope variables that are TYPE-COMPATIBLE with `expectedType`.
// A null/empty/gradual expected type offers every name (degrades to today's
// behaviour). The raw editor always remains as an escape hatch, so filtering
// here never makes a variable truly unreachable.
export function compatibleVarNames(availableVars, expectedType) {
  const vars = normalizeVars(availableVars);
  if ((expectedType ?? '').trim() === '') return vars.map((v) => v.name);
  return vars.filter((v) => typesCompatible(v.type, expectedType)).map((v) => v.name);
}

// --- expected-argument facets ----------------------------------------------

function expectedArgTypes(node) {
  return node?.data?.expectedArgTypes ?? [];
}

// The expected type for an exact slot path (e.g. `arg[0]`), or null.
export function expectedSlotType(node, slot) {
  const entry = expectedArgTypes(node).find((a) => a.slot === slot);
  return entry ? entry.type : null;
}

// The expected type for a call/effect record argument named `fieldName`. The
// lens emits its slot as `arg[0].<field>` for the common single-call node and
// `call[N].arg[M].<field>` when a node carries multiple receiver calls, so we
// match the canonical single-call slot first, then any slot whose tail is the
// field name.
export function expectedArgFieldType(node, fieldName) {
  const entries = expectedArgTypes(node);
  const exact = entries.find((a) => a.slot === `arg[0].${fieldName}`);
  if (exact) return exact.type;
  const tail = entries.find((a) => a.slot.endsWith(`.${fieldName}`));
  return tail ? tail.type : null;
}

// --- diagnostic facets -----------------------------------------------------

// Facet diagnostics are DEFINITE type errors (the lens only emits them for a
// concrete mismatch; gradual `any` never produces one), so any diagnostic is a
// blocking condition. Unknowns stay silent and never block.
export function nodeDiagnostics(node) {
  return node?.data?.diagnostics ?? [];
}

// Map every diagnostic in the document to its owning node id. Each node carries
// its own diagnostics (diagnostic.nodeId === the analyzed node's id), but we
// group defensively by the diagnostic's own `nodeId` so a diagnostic always
// lands on the node it names even if the backend attaches it elsewhere.
export function mapDiagnosticsToNodes(doc) {
  const byNode = new Map();
  for (const node of doc?.nodes ?? []) {
    for (const diag of nodeDiagnostics(node)) {
      const id = diag.nodeId ?? node.id;
      if (!byNode.has(id)) byNode.set(id, []);
      byNode.get(id).push(diag);
    }
  }
  return byNode;
}

// Every blocking diagnostic in the document as `[{ nodeId, kind, message }]`.
// Empty when facets are absent (older backend) — an un-analyzed graph never
// blocks Save.
export function blockingDiagnostics(doc) {
  if (!hasFacets(doc)) return [];
  const out = [];
  for (const [nodeId, diags] of mapDiagnosticsToNodes(doc)) {
    for (const diag of diags) {
      out.push({ nodeId, kind: diag.kind, message: diag.message });
    }
  }
  return out;
}

// Should Save be blocked? True only when the document carries facets AND at
// least one node has a definite-error diagnostic.
export function saveBlocked(doc) {
  return blockingDiagnostics(doc).length > 0;
}

// Drop every node's diagnostics from the draft. A client-side edit invalidates
// the last host derivation, so its diagnostics become STALE — and a stale error
// is worse than none (it points at text the user already changed). We clear
// them on edit so the graph reads as "unknown" (which never blocks Save) until
// the next Save re-derives accurate facets. Returns true when anything cleared.
export function clearFacetDiagnostics(doc) {
  let cleared = false;
  for (const node of doc?.nodes ?? []) {
    if (node?.data?.diagnostics?.length) {
      node.data.diagnostics = [];
      cleared = true;
    }
  }
  return cleared;
}

// A short, human-facing label for an expected type, or null when the type is
// gradual (`any`/`dict`/absent) and a hint would just be noise. Simplifies the
// structural `list[any]` we attach to iterables down to `list`.
export function describeExpectedType(type) {
  const t = (type ?? '').trim();
  if (t === '' || t === 'any' || t === 'dict') return null;
  if (t === 'list[any]') return 'list';
  return t;
}

// Best-effort node field a diagnostic points at, so the inline underline can
// land on the offending slot rather than the whole node. Node-level placement
// (by nodeId) is always available; this only refines a few well-known kinds.
export function diagnosticFieldHint(kind) {
  switch (kind) {
    case 'incompatible_iteration_target':
      return 'iterable';
    case 'incompatible_process_return':
      return 'expression';
    default:
      return null;
  }
}
