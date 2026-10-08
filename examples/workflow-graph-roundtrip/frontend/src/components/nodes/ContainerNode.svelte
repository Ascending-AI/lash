<script>
  import { Handle, Position } from '@xyflow/svelte';
  import { getContext } from 'svelte';
  import { kindMeta, containerSubkind, CONTAINER_SUBKINDS } from '../../lib/nodeKinds.js';
  import { groupOperations, operationMeta } from '../../lib/operations.js';
  import { clauseAdded, clauseRemoved } from '../../lib/fields.js';
  import ExpressionField from '../ExpressionField.svelte';
  import IdentifierField from '../IdentifierField.svelte';
  import { nodeDiagnostics } from '../../lib/facets.js';

  // A `for`/comprehension iterable is structurally a list: the lens rejects a
  // non-list target with a diagnostic. We surface that as a `list[any]` expected
  // type so the iterable's variable picker offers only list-typed (and gradual)
  // vars — a purely UI fact, not client-side Lashlang re-parsing.
  const ITERABLE_TYPE = 'list[any]';

  let { id, data } = $props();

  const run = getContext('run');
  const mode = getContext('mode');
  const ops = getContext('ops');

  // Which group's "+ Add node" menu is open (by slot), if any.
  let menuSlot = $state(null);
  function toggleMenu(slot) {
    menuSlot = menuSlot === slot ? null : slot;
  }
  function pickOperation(slot, op) {
    menuSlot = null;
    data.onAddNode?.(id, slot, op);
  }
  const opGroups = $derived(
    groupOperations(ops?.entries, { includePower: mode.power, topLevel: false }),
  );

  const node = $derived(data.node);
  const isProcess = $derived(node.data.kind === 'process');
  const meta = $derived(kindMeta(node.data.kind));
  const status = $derived(run.overlay[id] ?? null);
  const groups = $derived(data.groups ?? []);
  const availableVars = $derived(node.data.availableVars ?? []);
  const diagnostics = $derived(nodeDiagnostics(node));
  const hasDiag = $derived(diagnostics.length > 0);
  const reads = $derived(data.reads ?? []);
  const moveTargets = $derived(data.getMoveTargets ? data.getMoveTargets(id) : []);
  let moveOpen = $state(false);

  const subkind = $derived(isProcess ? null : containerSubkind(node));
  const subMeta = $derived(subkind ? (CONTAINER_SUBKINDS[subkind] ?? CONTAINER_SUBKINDS.loop) : null);
  const isWhile = $derived(subkind === 'while');
  const isIf = $derived(subkind === 'if');
  const isFor = $derived(subkind === 'for');
  const isComprehension = $derived(subkind === 'comprehension');
  const isConditional = $derived(isWhile || isIf);
  const clauses = $derived(node.data.clauses ?? []);
  const params = $derived(node.data.params ?? []);

  function commit() {
    data.onCommit?.();
  }
  function relayout() {
    (data.onRebuild ?? data.onCommit)?.();
  }
  // A process's canonical name is `data.name` (an identifier); its display title
  // mirrors it, so IdentifierField writes both together — renaming round-trips
  // into the source.
  function addParam() {
    node.data.params = [...(node.data.params ?? []), { name: 'arg', type: 'any' }];
    relayout();
  }
  function removeParam(i) {
    node.data.params = (node.data.params ?? []).filter((_, j) => j !== i);
    relayout();
  }
  // Comprehension clause add/remove. The lens rebuilds `data.clauses` verbatim,
  // so mutating the array is authoritative. Adding/removing a clause changes the
  // container's height, so route through onRebuild (commit + relayout).
  function addClause(kind) {
    node.data.clauses = clauseAdded(node.data.clauses, kind);
    (data.onRebuild ?? data.onCommit)?.();
  }
  function removeClause(index) {
    node.data.clauses = clauseRemoved(node.data.clauses, index);
    (data.onRebuild ?? data.onCommit)?.();
  }
</script>

<div
  class="wf-container"
  class:is-process={isProcess}
  class:is-running={status === 'running'}
  class:is-done={status === 'succeeded'}
  class:has-diag={hasDiag}
  style="--accent:{meta.accent}; width:{data.width}px; height:{data.height}px;"
>
  <Handle type="target" position={Position.Top} />

  <header class="ct-head">
    <span class="ct-badge"
      ><span class="ct-glyph">{isProcess ? meta.glyph : subMeta.glyph}</span>{isProcess
        ? 'process'
        : subMeta.label}</span
    >
    {#if isProcess}
      <IdentifierField
        value={node.data.name ?? node.data.title}
        variant="title"
        placeholder="process_name"
        ariaLabel="Process name"
        onInput={(v) => {
          node.data.name = v;
          node.data.title = v;
        }}
        onCommit={commit}
      />
    {:else if isConditional}
      <span class="ct-cond">
        <span class="ct-paren">(</span>
        <ExpressionField
          value={node.data.condition ?? ''}
          kind="expression"
          builder="comparison"
          {availableVars}
          placeholder="condition"
          onInput={(text) => (node.data.condition = text)}
          onCommit={commit}
        />
        <span class="ct-paren">)</span>
      </span>
    {:else if isFor}
      <span class="ct-for">
        <span class="ct-for-bind nodrag">
          <IdentifierField
            value={node.data.binding ?? ''}
            variant="box"
            placeholder="binding"
            ariaLabel="Loop binding"
            onInput={(v) => (node.data.binding = v)}
            onCommit={commit}
          />
        </span>
        <span class="ct-kw">in</span>
        <ExpressionField
          value={node.data.iterable ?? ''}
          kind="expression"
          builder="list"
          {availableVars}
          expectedType={ITERABLE_TYPE}
          placeholder="iterable"
          onInput={(text) => (node.data.iterable = text)}
          onCommit={commit}
        />
      </span>
    {:else if isComprehension}
      <span class="ct-for">
        <span class="ct-kw">let</span>
        <span class="ct-for-bind nodrag">
          <IdentifierField
            value={node.data.binding ?? ''}
            variant="box"
            placeholder="binding (optional)"
            ariaLabel="Comprehension binding"
            onInput={(v) => (node.data.binding = v === '' ? undefined : v)}
            onCommit={commit}
          />
        </span>
      </span>
    {:else}
      <span class="ct-title-static">{node.data.title}</span>
    {/if}
    {#if status}<span class="ct-status ct-status--{status}">{status}</span>{/if}
    {#if moveTargets.length}
      <span class="ct-move-wrap nodrag">
        <button
          class="ct-icon"
          class:is-open={moveOpen}
          title="Move to another scope"
          aria-label="Move branch to another scope"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            moveOpen = !moveOpen;
          }}>⤴</button
        >
        {#if moveOpen}
          <div class="ct-move-menu">
            <div class="ct-move-label">move into</div>
            {#each moveTargets as t (t.key)}
              <button
                class="ct-move-item"
                onpointerdown={(e) => e.stopPropagation()}
                onclick={(e) => {
                  e.stopPropagation();
                  moveOpen = false;
                  data.onMoveTo?.(id, t.dest);
                }}>{t.label}</button
              >
            {/each}
          </div>
        {/if}
      </span>
    {/if}
    {#if !isProcess && data.onReorder}
      <span class="ct-reorder nodrag" title="Reorder within scope">
        <button
          class="ct-move"
          title="Move earlier in execution order"
          aria-label="Move branch earlier"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            data.onReorder(id, 'up');
          }}>▲</button
        >
        <button
          class="ct-move"
          title="Move later in execution order"
          aria-label="Move branch later"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            data.onReorder(id, 'down');
          }}>▼</button
        >
      </span>
    {/if}
    <button
      class="ct-del"
      title={isProcess ? 'Delete process' : 'Delete branch'}
      aria-label={isProcess ? 'Delete process' : 'Delete branch'}
      onclick={(e) => {
        e.stopPropagation();
        data.onDelete(id);
      }}>×</button
    >
  </header>

  {#if reads.length}
    <div
      class="ct-reads nodrag"
      title={`reads variables from an enclosing scope: ${reads.join(', ')}`}
    >
      <span class="ct-reads-k">reads</span>
      {#each reads as v (v)}<span class="ct-reads-v">{v}</span>{/each}
    </div>
  {/if}

  {#if hasDiag}
    <div
      class="ct-diag nodrag"
      role="alert"
      title={diagnostics.map((d) => d.message).join('\n')}
    >
      <span class="ct-diag-mark">●</span>
      <span class="ct-diag-msg">{diagnostics[0].message}</span>
    </div>
  {/if}

  {#if isComprehension}
    <div class="ct-clauses nodrag">
      {#each clauses as clause, i (i)}
        <div class="ct-clause">
          {#if clause.kind === 'for'}
            <span class="ct-kw">for</span>
            <span class="ct-for-bind">
              <IdentifierField
                value={clause.binding ?? ''}
                variant="box"
                placeholder="binding"
                ariaLabel="Clause binding"
                onInput={(v) => (node.data.clauses[i].binding = v)}
                onCommit={commit}
              />
            </span>
            <span class="ct-kw">in</span>
            <ExpressionField
              value={clause.iterable ?? ''}
              kind="expression"
              builder="list"
              {availableVars}
              expectedType={ITERABLE_TYPE}
              placeholder="iterable"
              onInput={(text) => (node.data.clauses[i].iterable = text)}
              onCommit={commit}
            />
          {:else}
            <span class="ct-kw">if</span>
            <ExpressionField
              value={clause.condition ?? ''}
              kind="expression"
              builder="comparison"
              {availableVars}
              placeholder="condition"
              onInput={(text) => (node.data.clauses[i].condition = text)}
              onCommit={commit}
            />
          {/if}
          <button
            class="ct-clause-del"
            title="Remove clause"
            aria-label="Remove clause"
            onpointerdown={(e) => e.stopPropagation()}
            onclick={(e) => {
              e.stopPropagation();
              removeClause(i);
            }}>×</button
          >
        </div>
      {/each}
      <div class="ct-clause-add">
        <button
          class="ct-clause-addbtn"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            addClause('for');
          }}>+ for clause</button
        >
        <button
          class="ct-clause-addbtn"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            addClause('if');
          }}>+ if clause</button
        >
      </div>
    </div>
  {/if}

  {#if isProcess}
    <div class="ct-sig nodrag">
      <div class="ct-sig-group">
        <span class="ct-sig-label">params</span>
        {#each params as p, i (i)}
          <div class="ct-sig-row">
            <span class="ct-sig-name">
              <IdentifierField
                value={p.name}
                variant="bare"
                placeholder="name"
                ariaLabel="Parameter name"
                onInput={(v) => (node.data.params[i].name = v)}
                onCommit={commit}
              />
            </span>
            <span class="ct-sig-colon">:</span>
            <input
              class="ct-sig-type"
              value={p.type}
              oninput={(e) => (node.data.params[i].type = e.currentTarget.value)}
              onchange={commit}
              spellcheck="false"
              placeholder="type"
            />
            <button
              class="ct-sig-del"
              title="Remove parameter"
              aria-label="Remove parameter"
              onpointerdown={(e) => e.stopPropagation()}
              onclick={(e) => {
                e.stopPropagation();
                removeParam(i);
              }}>×</button
            >
          </div>
        {/each}
        <button
          class="ct-sig-add"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            addParam();
          }}>+ param</button
        >
      </div>
    </div>
  {/if}

  {#each groups as g (g.slot)}
    <span class="ct-slot" style="left:{g.x}px; top:{g.y - 4}px; width:{g.w}px;">{g.slot}</span>
  {/each}

  {#each groups as g (g.slot)}
    {#if g.addable && data.onAddNode}
      <div class="ct-add nodrag" style="left:{g.x}px; top:{g.y + g.h - 32}px; width:{g.w}px;">
        <button
          class="ct-add-btn"
          class:is-open={menuSlot === g.slot}
          title="Add a node to the end of this {g.slot}"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            toggleMenu(g.slot);
          }}
        >
          + Add node
        </button>
        {#if menuSlot === g.slot}
          <div class="ct-add-menu">
            {#if opGroups.length}
              {#each opGroups as grp (grp.id)}
                <div class="ct-add-group">{grp.label}</div>
                {#each grp.items as op (op.id)}
                  {@const m = operationMeta(op)}
                  <button
                    class="ct-add-item"
                    style="--c:{m.accent}"
                    onpointerdown={(e) => e.stopPropagation()}
                    onclick={(e) => {
                      e.stopPropagation();
                      pickOperation(g.slot, op);
                    }}
                  >
                    <span class="ct-add-glyph">{m.glyph}</span>{op.label}
                  </button>
                {/each}
              {/each}
            {:else}
              <div class="ct-add-empty">
                {ops?.error ? 'catalog unavailable — reload' : 'loading operations…'}
              </div>
            {/if}
          </div>
        {/if}
      </div>
    {/if}
  {/each}

  <Handle type="source" position={Position.Bottom} />
</div>

<style>
  .wf-container {
    position: relative;
    border: 1px solid color-mix(in srgb, var(--accent) 26%, var(--line));
    border-radius: 16px;
    background: color-mix(in srgb, var(--accent) 5%, rgba(12, 16, 24, 0.55));
    box-shadow: inset 0 0 60px -30px color-mix(in srgb, var(--accent) 60%, transparent);
    backdrop-filter: blur(1px);
  }
  .wf-container.is-process {
    border-style: solid;
    border-color: color-mix(in srgb, var(--accent) 22%, var(--line));
    background: rgba(10, 14, 21, 0.45);
  }
  .ct-head {
    position: absolute;
    top: 10px;
    left: 14px;
    right: 12px;
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .ct-badge {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-family: var(--font-mono);
    font-size: 9.5px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--accent) 34%, transparent);
    padding: 2px 7px;
    border-radius: 999px;
    white-space: nowrap;
    flex-shrink: 0;
  }
  .ct-glyph {
    font-size: 11px;
  }
  .ct-title {
    background: transparent;
    border: none;
    border-bottom: 1px dashed transparent;
    color: var(--text);
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 14px;
    padding: 1px 0;
    flex: 1;
    min-width: 0;
  }
  .ct-title:focus {
    outline: none;
    border-bottom-color: var(--accent);
  }
  .ct-title-static {
    color: var(--text-dim);
    font-family: var(--font-mono);
    font-size: 12px;
    flex: 1;
  }
  .ct-cond {
    display: flex;
    align-items: center;
    gap: 3px;
    flex: 1;
    min-width: 0;
  }
  .ct-paren {
    font-family: var(--font-mono);
    font-size: 14px;
    color: color-mix(in srgb, var(--accent) 75%, var(--text-dim));
    flex-shrink: 0;
  }
  .ct-for {
    display: flex;
    align-items: center;
    gap: 5px;
    flex: 1;
    min-width: 0;
  }
  .ct-for-bind {
    display: inline-flex;
    align-items: center;
    flex: 0 1 40%;
    min-width: 0;
  }
  .ct-kw {
    font-family: var(--font-mono);
    font-size: 11px;
    letter-spacing: 0.04em;
    color: color-mix(in srgb, var(--accent) 70%, var(--text-dim));
    flex-shrink: 0;
  }
  .ct-clauses {
    position: absolute;
    top: 38px;
    left: 14px;
    right: 12px;
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  .ct-clause {
    display: flex;
    align-items: center;
    gap: 5px;
    min-width: 0;
  }
  .ct-clause-del {
    border: none;
    background: transparent;
    color: var(--text-faint);
    font-size: 13px;
    line-height: 1;
    width: 17px;
    height: 17px;
    border-radius: 5px;
    padding: 0;
    cursor: pointer;
    flex-shrink: 0;
  }
  .ct-clause-del:hover {
    color: var(--rose);
    background: color-mix(in srgb, var(--rose) 16%, transparent);
  }
  .ct-clause-add {
    display: flex;
    gap: 5px;
  }
  .ct-clause-addbtn {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.03em;
    color: color-mix(in srgb, var(--accent) 80%, var(--text-dim));
    background: transparent;
    border: 1px dashed color-mix(in srgb, var(--accent) 40%, var(--line));
    border-radius: 6px;
    padding: 3px 8px;
    cursor: pointer;
    transition:
      color 0.12s ease,
      border-color 0.12s ease;
  }
  .ct-clause-addbtn:hover {
    color: var(--accent);
    border-color: var(--accent);
  }
  .ct-sig {
    position: absolute;
    top: 40px;
    left: 14px;
    right: 12px;
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .ct-sig-group {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 5px;
  }
  .ct-sig-label {
    font-family: var(--font-mono);
    font-size: 8.5px;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--text-faint);
    width: 44px;
    flex-shrink: 0;
  }
  .ct-sig-row {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    background: color-mix(in srgb, var(--accent) 8%, var(--ink-2));
    border: 1px solid color-mix(in srgb, var(--accent) 24%, var(--line));
    border-radius: 7px;
    padding: 2px 4px 2px 6px;
  }
  .ct-sig-name {
    display: inline-flex;
    align-items: center;
    min-width: 0;
    width: 52px;
  }
  .ct-sig-type {
    background: transparent;
    border: none;
    color: var(--text-dim);
    font-family: var(--font-mono);
    font-size: 10.5px;
    padding: 1px 0;
    min-width: 0;
    width: 40px;
  }
  .ct-sig-type:focus {
    outline: none;
  }
  .ct-sig-colon {
    color: var(--text-faint);
    font-family: var(--font-mono);
    font-size: 10px;
  }
  .ct-sig-del {
    border: none;
    background: transparent;
    color: var(--text-faint);
    font-size: 12px;
    line-height: 1;
    width: 15px;
    height: 15px;
    border-radius: 4px;
    padding: 0;
    cursor: pointer;
  }
  .ct-sig-del:hover {
    color: var(--rose);
    background: color-mix(in srgb, var(--rose) 16%, transparent);
  }
  .ct-sig-add {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.03em;
    color: color-mix(in srgb, var(--accent) 80%, var(--text-dim));
    background: transparent;
    border: 1px dashed color-mix(in srgb, var(--accent) 40%, var(--line));
    border-radius: 6px;
    padding: 2px 8px;
    cursor: pointer;
    transition:
      color 0.12s ease,
      border-color 0.12s ease;
  }
  .ct-sig-add:hover {
    color: var(--accent);
    border-color: var(--accent);
  }
  .ct-reads {
    position: absolute;
    top: 38px;
    left: 14px;
    right: 12px;
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 5px;
  }
  .ct-reads-k {
    font-family: var(--font-mono);
    font-size: 8.5px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: color-mix(in srgb, var(--cyan) 80%, var(--text-dim));
  }
  .ct-reads-v {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--cyan);
    background: color-mix(in srgb, var(--cyan) 12%, transparent);
    border-radius: 5px;
    padding: 1px 6px;
  }
  .ct-status {
    font-family: var(--font-mono);
    font-size: 8.5px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    padding: 2px 5px;
    border-radius: 5px;
    color: var(--cyan);
    background: color-mix(in srgb, var(--cyan) 18%, transparent);
    flex-shrink: 0;
  }
  .ct-status--succeeded {
    color: #5fd08a;
    background: rgba(95, 208, 138, 0.16);
  }
  .ct-del {
    border: none;
    background: transparent;
    color: var(--text-faint);
    font-size: 17px;
    width: 20px;
    height: 20px;
    border-radius: 6px;
    padding: 0;
    flex-shrink: 0;
  }
  .ct-del:hover {
    color: var(--rose);
    background: color-mix(in srgb, var(--rose) 16%, transparent);
  }
  .ct-move-wrap {
    position: relative;
    flex-shrink: 0;
  }
  .ct-icon {
    border: none;
    background: transparent;
    color: var(--text-faint);
    font-size: 12px;
    line-height: 1;
    width: 20px;
    height: 20px;
    border-radius: 6px;
    padding: 0;
    transition:
      color 0.15s ease,
      background 0.15s ease;
  }
  .ct-icon:hover,
  .ct-icon.is-open {
    color: var(--cyan);
    background: color-mix(in srgb, var(--cyan) 16%, transparent);
  }
  .ct-move-menu {
    position: absolute;
    top: calc(100% + 5px);
    right: 0;
    z-index: 30;
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 150px;
    max-height: 200px;
    overflow-y: auto;
    padding: 5px;
    background: var(--ink-2);
    border: 1px solid var(--line-strong);
    border-radius: 9px;
    box-shadow: 0 14px 30px -12px rgba(0, 0, 0, 0.7);
  }
  .ct-move-label {
    font-family: var(--font-mono);
    font-size: 8px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--text-faint);
    padding: 2px 6px 4px;
  }
  .ct-move-item {
    text-align: left;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    color: var(--text-dim);
    font-family: var(--font-mono);
    font-size: 10.5px;
    padding: 4px 7px;
    cursor: pointer;
  }
  .ct-move-item:hover {
    color: var(--text);
    background: color-mix(in srgb, var(--cyan) 12%, transparent);
    border-color: color-mix(in srgb, var(--cyan) 40%, transparent);
  }
  .ct-reorder {
    display: inline-flex;
    flex-direction: column;
    gap: 1px;
    flex-shrink: 0;
  }
  .ct-move {
    border: none;
    background: transparent;
    color: var(--text-faint);
    font-size: 8px;
    line-height: 1;
    width: 17px;
    height: 10px;
    padding: 0;
    border-radius: 3px;
    cursor: pointer;
    transition:
      color 0.15s ease,
      background 0.15s ease;
  }
  .ct-move:hover {
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 16%, transparent);
  }
  .ct-slot {
    position: absolute;
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--text-faint);
    border-top: 1px dashed color-mix(in srgb, var(--accent) 30%, transparent);
    padding-top: 3px;
    text-align: center;
    pointer-events: none;
  }
  .wf-container.has-diag {
    border-color: color-mix(in srgb, var(--rose) 55%, var(--line));
    box-shadow:
      inset 0 0 70px -34px color-mix(in srgb, var(--rose) 55%, transparent),
      0 0 0 1px color-mix(in srgb, var(--rose) 38%, transparent);
  }
  .ct-diag {
    position: absolute;
    top: 34px;
    left: 14px;
    right: 12px;
    z-index: 6;
    display: flex;
    align-items: center;
    gap: 6px;
    padding: 4px 8px;
    background: color-mix(in srgb, var(--rose) 16%, rgba(10, 14, 21, 0.92));
    border: 1px solid color-mix(in srgb, var(--rose) 42%, transparent);
    border-radius: 7px;
  }
  .ct-diag-mark {
    color: var(--rose);
    font-size: 8px;
    flex-shrink: 0;
  }
  .ct-diag-msg {
    font-family: var(--font-mono);
    font-size: 9.5px;
    line-height: 1.35;
    color: #ffb4c6;
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
  }

  .wf-container.is-running {
    border-color: var(--cyan);
    box-shadow:
      inset 0 0 70px -34px color-mix(in srgb, var(--cyan) 70%, transparent),
      0 0 0 1px color-mix(in srgb, var(--cyan) 40%, transparent);
  }

  .ct-add {
    position: absolute;
    display: flex;
    justify-content: center;
    z-index: 3;
  }
  .ct-add-btn {
    font-family: var(--font-mono);
    font-size: 10px;
    letter-spacing: 0.04em;
    color: color-mix(in srgb, var(--accent) 80%, var(--text-dim));
    background: color-mix(in srgb, var(--accent) 8%, rgba(10, 14, 21, 0.85));
    border: 1px dashed color-mix(in srgb, var(--accent) 45%, var(--line));
    border-radius: 8px;
    padding: 5px 12px;
    cursor: pointer;
    transition:
      color 0.15s ease,
      background 0.15s ease,
      border-color 0.15s ease;
  }
  .ct-add-btn:hover,
  .ct-add-btn.is-open {
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 16%, rgba(10, 14, 21, 0.92));
    border-style: solid;
    border-color: var(--accent);
  }
  .ct-add-menu {
    position: absolute;
    top: calc(100% + 5px);
    left: 50%;
    transform: translateX(-50%);
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 6px;
    min-width: 200px;
    max-height: 300px;
    overflow-y: auto;
    background: var(--ink-2, #0d1119);
    border: 1px solid var(--line-strong, #2a3346);
    border-radius: 10px;
    box-shadow: 0 14px 34px -12px rgba(0, 0, 0, 0.7);
    z-index: 20;
  }
  .ct-add-group {
    font-family: var(--font-mono);
    font-size: 8px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--text-faint);
    padding: 5px 7px 2px;
  }
  .ct-add-empty {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--text-faint);
    padding: 8px 7px;
    text-align: center;
  }
  .ct-add-item {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-family: var(--font-mono);
    font-size: 10.5px;
    color: var(--text-dim);
    background: transparent;
    border: 1px solid transparent;
    border-radius: 7px;
    padding: 5px 7px;
    cursor: pointer;
    text-align: left;
    transition:
      color 0.12s ease,
      background 0.12s ease,
      border-color 0.12s ease;
  }
  .ct-add-item:hover {
    color: var(--text);
    background: color-mix(in srgb, var(--c) 14%, transparent);
    border-color: color-mix(in srgb, var(--c) 45%, transparent);
  }
  .ct-add-glyph {
    color: var(--c);
    font-size: 11px;
    width: 13px;
    text-align: center;
    flex-shrink: 0;
  }
</style>
