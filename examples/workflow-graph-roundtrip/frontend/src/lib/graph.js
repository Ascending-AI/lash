import { layoutDocument } from './layout.js';
import { defaultSource, fieldDefaultValue } from './operations.js';

// Build SvelteFlow nodes + edges from a draft WorkflowDocument.
//
// The draft document is the source of truth (the host owns the mutable draft).
// Each SvelteFlow node's `data.node` is a live reference into the draft, so an
// edit inside a node component mutates the draft that Save serializes back.
// Positions come from auto-layout, overridden by any user-dragged position
// persisted out-of-band in localStorage.
export function buildFlow(doc, storedPositions, handlers = {}) {
  const { onDelete, onAddNode, onReorder, onCommit, onRebuild, onMoveTo, getMoveTargets } =
    handlers;
  const { positions, sizes, groupLayouts } = layoutDocument(doc);
  const nodeMap = new Map(doc.nodes.map((n) => [n.id, n]));

  // Emit parents before children — SvelteFlow requires it for nesting.
  const ordered = [];
  const seen = new Set();
  function emit(id) {
    if (seen.has(id)) return;
    const node = nodeMap.get(id);
    if (!node) return;
    seen.add(id);
    ordered.push(node);
    for (const grp of node.data.children ?? []) {
      for (const childId of grp.nodeIds) emit(childId);
    }
  }
  for (const id of doc.roots?.processes ?? []) emit(id);
  for (const id of doc.roots?.main ?? []) emit(id);
  for (const node of doc.nodes) emit(node.id); // defensive: any strays

  const flowNodes = ordered.map((node) => {
    const size = sizes.get(node.id) ?? { w: 260, h: 90 };
    const stored = storedPositions[node.id];
    const auto = positions.get(node.id) ?? { x: 0, y: 0 };
    const isContainer = (node.data.children ?? []).length > 0;
    return {
      id: node.id,
      type: isContainer ? 'container' : node.data.kind === 'opaque' ? 'opaque' : 'workflow',
      position: stored ? { x: stored.x, y: stored.y } : { x: auto.x, y: auto.y },
      parentId: node.parentId,
      extent: node.parentId ? 'parent' : undefined,
      width: size.w,
      height: size.h,
      draggable: true,
      selectable: true,
      data: {
        node,
        width: size.w,
        height: size.h,
        groups: groupLayouts.get(node.id) ?? null,
        onDelete,
        onAddNode,
        onReorder,
        onCommit,
        onRebuild,
        onMoveTo,
        getMoveTargets,
      },
      // Containers must sit behind their children.
      zIndex: isContainer ? 0 : 1,
    };
  });

  // Classify data edges by scope. A data edge is "same-scope" when its source
  // and target share the same parent container/process group (identical
  // parentId — two top-level nodes both read as `null` and count as same
  // scope). Same-scope data edges route cleanly under dagre and keep their
  // variable-name pill.
  //
  // Cross-scope data edges — a variable defined in one scope but consumed
  // inside a nested container (e.g. `state`, defined in the process body, read
  // inside the `while` body) — cannot be routed by the per-scope layout, so
  // SvelteFlow would draw them as floating dashed lines whose label chips land
  // on top of node headers and borders. We do not draw them; instead we surface
  // the dependency as an in-node "reads: …" chip on the consuming node, so the
  // data flow stays legible without spaghetti.
  const parentOf = new Map(doc.nodes.map((n) => [n.id, n.parentId ?? null]));
  const readsByNode = new Map();
  const flowEdges = [];
  for (const edge of doc.edges) {
    const isData = edge.data?.kind === 'data';
    if (isData && parentOf.get(edge.source) !== parentOf.get(edge.target)) {
      const variable = edge.data?.variable;
      if (variable) {
        let set = readsByNode.get(edge.target);
        if (!set) readsByNode.set(edge.target, (set = new Set()));
        set.add(variable);
      }
      continue; // cross-scope data edge — surfaced as a chip, not an arrow
    }
    flowEdges.push({
      id: edge.id,
      source: edge.source,
      target: edge.target,
      type: 'smoothstep',
      animated: false,
      label: isData ? edge.data?.variable ?? 'data' : undefined,
      data: { kind: edge.data?.kind ?? 'sequence' },
      class: isData ? 'edge-data' : 'edge-sequence',
      zIndex: 2,
      markerEnd: { type: 'arrowclosed', width: 16, height: 16 },
    });
  }

  // Attach cross-scope "reads" to each consuming node so it can render a chip.
  for (const fn of flowNodes) {
    const reads = readsByNode.get(fn.id);
    if (reads && reads.size) fn.data.reads = [...reads];
  }

  return { flowNodes, flowEdges };
}

// ---------------------------------------------------------------------------
// Adding nodes.
//
// New nodes are minted client-side with temporary `new:<n>` ids. The backend
// treats any posted flow node whose id is not in its stored baseline as NEW: it
// builds a fresh node from `data.kind` (+ `data.subkind` for containers) and the
// editable field text below, then re-projects and returns a canonical document
// with real ids — so these temp ids only ever live inside the draft.
// ---------------------------------------------------------------------------

let nodeCounter = 0;
let edgeCounter = 0;

function mintNodeId(taken) {
  let id;
  do {
    id = `new:${(nodeCounter += 1)}`;
  } while (taken.has(id));
  taken.add(id);
  return id;
}

function mintEdgeId(taken) {
  let id;
  do {
    id = `new-edge:${(edgeCounter += 1)}`;
  } while (taken.has(id));
  taken.add(id);
  return id;
}

// --- Building a node from an operation-catalog entry -----------------------
//
// The operation catalog (GET /operations) is the sole data home for what a
// palette can insert. An entry is
//   { id, label, nodeKind, subkind?, operation?, effect?, terminalKind?,
//     fields:[{ name, type, default }] }.
// `nodeDataFromOperation` turns one entry into the node `data` payload the lens
// accepts: value-bearing leaves (call/effect/terminal/data) carry a canonical
// `expression`; structured nodes carry their text slots; call/effect also seed
// `data.fields` so their arguments are editable the instant they are inserted.

// A field's default as canonical (unquoted) slot text.
function slotText(field) {
  if (!field) return '';
  if (field.type === 'number') return String(Number(field.default ?? 0) || 0);
  if (field.type === 'boolean') return field.default ? 'true' : 'false';
  return defaultSource(field.default);
}

function seedFields(fields) {
  const out = {};
  for (const field of fields ?? []) out[field.name] = fieldDefaultValue(field);
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
function synthCallExpression(op) {
  const args = (op.fields ?? []).map(recordArg).join(', ');
  // The receiver comes from the catalog entry, not from the operation name:
  // `list_recent` belongs to `gmail`, and synthesizing `display.list_recent`
  // gives the lowerer a receiver that has no such operation (FIG-3178).
  return `await ${op.receiver ?? 'display'}.${op.operation}({ ${args} })`;
}

function synthEffectExpression(op, byName) {
  if (op.effect === 'sleep') return `sleep for ${slotText(byName.duration) || '"1s"'}`;
  if (op.effect === 'wait_signal') {
    return `wait_signal(${JSON.stringify(String(byName.signal?.default ?? 'continue'))})`;
  }
  return slotText(byName.expression) || 'sleep for "1s"';
}

function nodeDataFromOperation(op) {
  const kind = op.nodeKind;
  const data = { kind, title: op.label ?? kind, nameSource: 'derived' };
  if (op.subkind) data.subkind = op.subkind;
  if (op.operation) data.operation = op.operation;
  if (op.effect) data.effect = op.effect;
  if (op.terminalKind) data.terminalKind = op.terminalKind;
  const byName = Object.fromEntries((op.fields ?? []).map((f) => [f.name, f]));

  switch (kind) {
    case 'opaque':
      data.source = String(byName.source?.default ?? '');
      break;
    case 'call':
      // The receiver rides on the node too, so a save that omits the
      // expression is synthesized against the same receiver by the backend.
      if (op.receiver) data.receiver = op.receiver;
      data.expression = synthCallExpression(op);
      data.fields = seedFields(op.fields);
      break;
    case 'effect':
      data.expression = synthEffectExpression(op, byName);
      data.fields = seedFields(op.fields);
      break;
    case 'terminal':
      // The lens wraps this value with the finish/fail keyword from
      // `terminalKind` (set above), so seed the bare value only.
      data.expression = slotText(byName.expression) || '0';
      break;
    case 'data':
    case 'computation': {
      const binding = slotText(byName.binding);
      if (binding) data.binding = binding;
      data.expression = slotText(byName.expression) || '0';
      break;
    }
    case 'state_update':
      data.target = slotText(byName.target) || 'state.count';
      data.expression = slotText(byName.expression) || '0';
      break;
    case 'container':
      if (op.subkind === 'if' || op.subkind === 'while') {
        data.condition = slotText(byName.condition) || (op.subkind === 'while' ? 'false' : 'true');
      } else if (op.subkind === 'for') {
        data.binding = slotText(byName.binding) || 'item';
        data.iterable = slotText(byName.iterable) || '[1, 2, 3]';
      } else if (op.subkind === 'comprehension') {
        const binding = slotText(byName.binding);
        if (binding) data.binding = binding;
        data.clauses = [{ kind: 'for', binding: 'x', iterable: '[1, 2, 3]' }];
      }
      break;
    default:
      break;
  }
  return data;
}

// A ready-to-run default child seeded into a fresh container slot: the catalog's
// first action (`call`) operation, except a comprehension's single-expression
// `element` slot (a bare value). Falls back to a minimal call if the catalog is
// empty (adding is only reachable once the catalog has loaded, so this is rare).
function seedChildFor(subkind, catalog) {
  if (subkind === 'comprehension') {
    return { kind: 'data', title: 'x', nameSource: 'derived', expression: 'x' };
  }
  const action = (catalog ?? []).find((op) => op.nodeKind === 'call');
  if (action) return nodeDataFromOperation(action);
  return {
    kind: 'call',
    title: 'Show message',
    nameSource: 'derived',
    operation: 'show_message',
    expression: 'await display.show_message({ text: "" })',
    fields: { text: '' },
  };
}

// Container slot layout per subkind: which slots exist and which one is seeded
// with the initial child. `else` is left empty; the comprehension `element` slot
// must hold exactly one node, so it is never an insertion target (see below).
function containerSlots(kind) {
  switch (kind) {
    case 'if':
      return [{ slot: 'then', seeded: true }];
    case 'while':
    case 'for':
      return [{ slot: 'body', seeded: true }];
    case 'comprehension':
      return [{ slot: 'element', seeded: true }];
    default:
      return [];
  }
}

// Resolve an insertion target to the concrete list its new node id joins, the
// parentId to stamp, and the scope string sequence edges in that group carry.
// A target is either { main: true } or { ownerId, slot }.
function resolveGroup(doc, target) {
  if (target?.main) {
    doc.roots ??= { main: [], processes: [] };
    doc.roots.main ??= [];
    return { nodeIds: doc.roots.main, parentId: undefined, scope: 'main' };
  }
  const owner = doc.nodes.find((n) => n.id === target.ownerId);
  const group = (owner?.data.children ?? []).find((g) => g.slot === target.slot);
  if (!group) return null;
  return { nodeIds: group.nodeIds, parentId: owner.id, scope: group.scope };
}

// Insert a new top-level process from the `proc.process` catalog entry. It joins
// `roots.processes` with a `body` slot seeded with one default action node (an
// empty body is also accepted — the backend seeds `finish 0`). The process's
// name comes from the catalog `name` field default. Mutates `doc`; returns the
// new process id.
export function addProcessToDoc(doc, operation, catalog = []) {
  const taken = new Set(doc.nodes.map((n) => n.id));
  const id = mintNodeId(taken);
  const nameField = (operation.fields ?? []).find((f) => f.name === 'name');
  const name = String(nameField?.default ?? 'my_process');

  const childId = mintNodeId(taken);
  const childData = seedChildFor(null, catalog);
  doc.nodes.push({ id: childId, type: childData.kind, parentId: id, data: childData });

  doc.nodes.push({
    id,
    type: 'process',
    data: {
      kind: 'process',
      title: name,
      name,
      nameSource: 'derived',
      params: [],
      signals: [],
      children: [{ slot: 'body', scope: `process:${id}`, nodeIds: [childId] }],
    },
  });
  doc.roots ??= { main: [], processes: [] };
  doc.roots.processes ??= [];
  doc.roots.processes.push(id);
  return id;
}

// Insert a new node built from an operation-catalog entry into the target
// group. Mirrors deleteNodeFromDoc's draft-mutation style: splice the id into
// the group's nodeIds (BEFORE a trailing terminal so the new node isn't dead
// code after a `finish`/`fail`), push the FlowNode, set parentId to match how
// buildFlow reads membership, and chain sequence edges around the new position.
// Container entries also get their slot group(s) + one seeded child. Data edges
// are never added here — the backend recomputes them on reproject. Mutates
// `doc`; returns the new node id (or null if the target could not be resolved).
export function addNodeToDoc(doc, target, operation, catalog = []) {
  // A process is a top-level declaration, not a body statement: it joins
  // `roots.processes` with its own body slot rather than an owner's group.
  if (operation.nodeKind === 'process') return addProcessToDoc(doc, operation, catalog);

  const taken = new Set(doc.nodes.map((n) => n.id));
  const takenEdges = new Set(doc.edges.map((e) => e.id));
  const group = resolveGroup(doc, target);
  if (!group) return null;

  const id = mintNodeId(taken);
  const data = nodeDataFromOperation(operation);

  // Seed container slot groups (+ their initial child nodes) before appending.
  if (data.kind === 'container') {
    data.children = [];
    for (const { slot, seeded } of containerSlots(operation.subkind)) {
      const nodeIds = [];
      if (seeded) {
        const childId = mintNodeId(taken);
        const childData = seedChildFor(operation.subkind, catalog);
        doc.nodes.push({
          id: childId,
          type: childData.kind,
          parentId: id,
          data: childData,
        });
        nodeIds.push(childId);
      }
      data.children.push({ slot, scope: `container:${id}:${slot}`, nodeIds });
    }
  }

  const flowNode = { id, type: data.kind, data };
  if (group.parentId !== undefined) flowNode.parentId = group.parentId;
  doc.nodes.push(flowNode);

  // Source order == nodeIds order (the backend renders the block in node order),
  // so a new node appended after a trailing `terminal` (`finish`/`fail`) would be
  // dead code. Insert BEFORE a trailing terminal; otherwise append.
  const ids = group.nodeIds;
  const kindOf = new Map(doc.nodes.map((n) => [n.id, n.data?.kind]));
  const lastId = ids[ids.length - 1];
  const trailingTerminal = lastId !== undefined && kindOf.get(lastId) === 'terminal';
  const insertIndex = trailingTerminal ? ids.length - 1 : ids.length;
  const before = insertIndex > 0 ? ids[insertIndex - 1] : undefined; // now precedes new
  const after = insertIndex < ids.length ? ids[insertIndex] : undefined; // now follows new
  ids.splice(insertIndex, 0, id);

  // Chain sequence edges so ordering is explicit (the backend recomputes them on
  // reproject, but keep the draft sane — no edge skipping the new node).
  if (before !== undefined && after !== undefined) {
    // Was before → after; drop that so we can splice new between them.
    doc.edges = doc.edges.filter(
      (e) =>
        !(
          e.data?.kind === 'sequence' &&
          e.data?.scope === group.scope &&
          e.source === before &&
          e.target === after
        ),
    );
  }
  if (before !== undefined) {
    doc.edges.push({
      id: mintEdgeId(takenEdges),
      source: before,
      target: id,
      data: { kind: 'sequence', scope: group.scope },
    });
  }
  if (after !== undefined) {
    doc.edges.push({
      id: mintEdgeId(takenEdges),
      source: id,
      target: after,
      data: { kind: 'sequence', scope: group.scope },
    });
  }
  return id;
}

// Remove a node from the draft document: drop it from nodes, from every root /
// child group that references it, and drop incident edges. Mutates `doc`.
export function deleteNodeFromDoc(doc, nodeId) {
  const node = doc.nodes.find((n) => n.id === nodeId);
  if (!node) return;
  // Collect the node and all its descendants (deleting a container prunes its subtree).
  const toRemove = new Set();
  const nodeMap = new Map(doc.nodes.map((n) => [n.id, n]));
  (function collect(id) {
    if (toRemove.has(id)) return;
    toRemove.add(id);
    const n = nodeMap.get(id);
    for (const grp of n?.data.children ?? []) for (const c of grp.nodeIds) collect(c);
  })(nodeId);

  doc.nodes = doc.nodes.filter((n) => !toRemove.has(n.id));
  doc.edges = doc.edges.filter((e) => !toRemove.has(e.source) && !toRemove.has(e.target));
  if (doc.roots) {
    doc.roots.main = (doc.roots.main ?? []).filter((id) => !toRemove.has(id));
    doc.roots.processes = (doc.roots.processes ?? []).filter((id) => !toRemove.has(id));
  }
  for (const n of doc.nodes) {
    for (const grp of n.data.children ?? []) {
      grp.nodeIds = grp.nodeIds.filter((id) => !toRemove.has(id));
    }
  }
}

// ---------------------------------------------------------------------------
// Reordering statements within a scope.
//
// Execution order == the order of ids in a group's `nodeIds`; the backend
// renders source from that order and reprojects. Locate the group that owns
// `nodeId` (the top-level `main` root or a container/process child group),
// then move the id within that list. Mirrors deleteNodeFromDoc's
// draft-mutation style. Sequence edges within the scope are re-chained locally
// for immediate visual correctness — the backend recomputes them regardless on
// reproject. Mutates `doc`; returns true if the order actually changed.
// ---------------------------------------------------------------------------

// Resolve the group list (and its sequence-edge scope) that contains `nodeId`.
// Top-level `main` nodes carry the `main` scope; processes are parallel
// top-level definitions with no sequence ordering (scope null, no re-chain).
function findGroupOf(doc, nodeId) {
  const main = doc.roots?.main ?? [];
  if (main.includes(nodeId)) return { nodeIds: main, scope: 'main' };
  const procs = doc.roots?.processes ?? [];
  if (procs.includes(nodeId)) return { nodeIds: procs, scope: null };
  for (const n of doc.nodes) {
    for (const grp of n.data.children ?? []) {
      if (grp.nodeIds.includes(nodeId)) return { nodeIds: grp.nodeIds, scope: grp.scope ?? null };
    }
  }
  return null;
}

// Drop every sequence edge in `scope` and re-chain consecutive members of the
// (already reordered) group so the drawn arrows match the new order.
function rechainSequenceEdges(doc, group) {
  const scope = group.scope;
  if (!scope) return;
  const ids = group.nodeIds;
  doc.edges = doc.edges.filter((e) => !(e.data?.kind === 'sequence' && e.data?.scope === scope));
  const takenEdges = new Set(doc.edges.map((e) => e.id));
  for (let i = 0; i + 1 < ids.length; i += 1) {
    doc.edges.push({
      id: mintEdgeId(takenEdges),
      source: ids[i],
      target: ids[i + 1],
      data: { kind: 'sequence', scope },
    });
  }
}

// Move `nodeId` within its scope. `direction` is 'up' | 'down' or a target
// index. A trailing terminal (`finish`/`fail`) is a barrier: a non-terminal
// node cannot move after it (clamp), and a terminal itself never moves.
export function reorderNodeInDoc(doc, nodeId, direction) {
  const group = findGroupOf(doc, nodeId);
  if (!group) return false;
  const ids = group.nodeIds;
  const from = ids.indexOf(nodeId);
  if (from === -1) return false;

  const kindOf = new Map(doc.nodes.map((n) => [n.id, n.data?.kind]));
  if (kindOf.get(nodeId) === 'terminal') return false; // terminals stay put

  // A trailing terminal reserves the last slot; movable nodes stop before it.
  const lastId = ids[ids.length - 1];
  const hasTrailingTerminal = lastId !== undefined && kindOf.get(lastId) === 'terminal';
  const maxIndex = hasTrailingTerminal ? ids.length - 2 : ids.length - 1;

  let to = direction === 'up' ? from - 1 : direction === 'down' ? from + 1 : Number(direction);
  if (Number.isNaN(to)) return false;
  to = Math.max(0, Math.min(to, maxIndex));
  if (to === from) return false;

  ids.splice(from, 1);
  ids.splice(to, 0, nodeId);
  rechainSequenceEdges(doc, group);
  return true;
}

// ---------------------------------------------------------------------------
// Moving a node into a different scope (container slot ↔ top-level main).
//
// The backend supports move-scope through membership: a node's parentId +
// which group's nodeIds it belongs to determine its owning block. We update
// that membership + parentId and re-chain the sequence edges in both the source
// and destination scopes. Data edges are recomputed by the backend on reproject.
// ---------------------------------------------------------------------------

// A container's whole descendant subtree (used to forbid moving it into itself).
function descendantsOf(doc, nodeId) {
  const map = new Map(doc.nodes.map((n) => [n.id, n]));
  const out = new Set();
  (function walk(id) {
    const node = map.get(id);
    for (const grp of node?.data.children ?? []) {
      for (const child of grp.nodeIds) {
        if (!out.has(child)) {
          out.add(child);
          walk(child);
        }
      }
    }
  })(nodeId);
  return out;
}

// Resolve a destination descriptor ({ main:true } | { ownerId, slot }) to its
// concrete nodeIds list, sequence scope, and parentId (undefined for main).
function resolveDestination(doc, dest) {
  if (dest?.main) {
    doc.roots ??= { main: [], processes: [] };
    doc.roots.main ??= [];
    return { nodeIds: doc.roots.main, scope: 'main', parentId: undefined };
  }
  const owner = doc.nodes.find((n) => n.id === dest?.ownerId);
  const group = (owner?.data.children ?? []).find((g) => g.slot === dest?.slot);
  if (!group) return null;
  return { nodeIds: group.nodeIds, scope: group.scope, parentId: owner.id };
}

// Enumerate the scopes a node may move INTO: top-level `main`, plus every
// insertable container slot that is not the node's current scope nor inside the
// node's own subtree. `element` slots (comprehension) hold exactly one node and
// are excluded. Returns `[{ key, label, dest }]` for a "move to…" menu.
export function moveTargetsFor(doc, nodeId) {
  const node = doc.nodes.find((n) => n.id === nodeId);
  // Terminals stay put; processes are always top-level and never move into a scope.
  if (!node || node.data?.kind === 'terminal' || node.data?.kind === 'process') return [];
  const own = descendantsOf(doc, nodeId);
  own.add(nodeId);
  const current = findGroupOf(doc, nodeId);
  const targets = [];
  const mainIds = doc.roots?.main ?? [];
  if (!(current && current.nodeIds === mainIds)) {
    targets.push({ key: 'main', label: 'top level', dest: { main: true } });
  }
  for (const owner of doc.nodes) {
    if (own.has(owner.id)) continue;
    for (const grp of owner.data?.children ?? []) {
      if (grp.slot === 'element') continue;
      if (current && grp.nodeIds === current.nodeIds) continue;
      const ownerLabel = owner.data?.title?.trim() || owner.data?.subkind || owner.data?.kind;
      targets.push({
        key: `${owner.id}:${grp.slot}`,
        label: `${ownerLabel} · ${grp.slot}`,
        dest: { ownerId: owner.id, slot: grp.slot },
      });
    }
  }
  return targets;
}

// Move `nodeId` into `dest`, inserting at `insertIndex` (clamped before a
// trailing terminal). A same-scope destination degrades to a reorder. A
// container cannot move into its own subtree. Mutates `doc`; returns true when
// the document actually changed.
export function moveNodeToGroup(doc, nodeId, dest, insertIndex = Infinity) {
  const node = doc.nodes.find((n) => n.id === nodeId);
  if (!node) return false;
  const kindOf = new Map(doc.nodes.map((n) => [n.id, n.data?.kind]));
  if (kindOf.get(nodeId) === 'terminal') return false;

  if (dest && !dest.main) {
    if (dest.ownerId === nodeId) return false;
    if (descendantsOf(doc, nodeId).has(dest.ownerId)) return false;
  }

  const target = resolveDestination(doc, dest);
  if (!target) return false;

  const source = findGroupOf(doc, nodeId);
  if (!source) return false;
  if (source.nodeIds === target.nodeIds) {
    return reorderNodeInDoc(doc, nodeId, insertIndex);
  }

  const from = source.nodeIds.indexOf(nodeId);
  if (from === -1) return false;
  source.nodeIds.splice(from, 1);

  const destIds = target.nodeIds;
  const lastId = destIds[destIds.length - 1];
  const trailingTerminal = lastId !== undefined && kindOf.get(lastId) === 'terminal';
  const maxIndex = trailingTerminal ? destIds.length - 1 : destIds.length;
  let at = Number.isFinite(insertIndex) ? insertIndex : destIds.length;
  at = Math.max(0, Math.min(at, maxIndex));
  destIds.splice(at, 0, nodeId);

  if (target.parentId === undefined) delete node.parentId;
  else node.parentId = target.parentId;

  rechainSequenceEdges(doc, source);
  rechainSequenceEdges(doc, { nodeIds: target.nodeIds, scope: target.scope });
  return true;
}

// The sequence scope + ordered sibling ids that contain `nodeId` (for
// drag-to-reorder hit-testing in the host). Returns null for unsequenced
// scopes (parallel processes) where order is meaningless.
export function scopeOf(doc, nodeId) {
  const group = findGroupOf(doc, nodeId);
  if (!group || !group.scope) return null;
  return { scope: group.scope, nodeIds: group.nodeIds.slice() };
}
