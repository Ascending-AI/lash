<script>
  import { Handle, Position } from '@xyflow/svelte';
  import { getContext } from 'svelte';
  import { kindMeta, WAITING_EFFECTS } from '../../lib/nodeKinds.js';
  import {
    operationsForKind,
    currentOperationId,
    fieldDefaultValue,
    operationSwitchPatch,
  } from '../../lib/operations.js';
  import ExpressionField from '../ExpressionField.svelte';
  import {
    expectedArgFieldType,
    nodeDiagnostics,
    enumMembers,
    enumMemberFromText,
    describeExpectedType,
    diagnosticFieldHint,
  } from '../../lib/facets.js';

  let { id, data } = $props();

  const run = getContext('run');
  const mode = getContext('mode');
  const ops = getContext('ops');
  const node = $derived(data.node);
  const meta = $derived(kindMeta(node.data.kind));
  const status = $derived(run.overlay[id] ?? null);

  const kind = $derived(node.data.kind);
  const isAssign = $derived(kind === 'state_update');
  const isComputation = $derived(kind === 'computation');
  const isData = $derived(kind === 'data');
  const isTerminal = $derived(kind === 'terminal');
  // call / effect are configured through their typed `fields` (record args /
  // duration / signal) + an optional `let <binding> =` name; their raw call
  // expression is rebuilt by the lens, so it is not shown as an input here.
  const isInvoke = $derived(kind === 'call' || kind === 'effect');
  const hasBinding = $derived(isInvoke || isData);
  // state_update / computation always carry an editable expression; data /
  // terminal expose one only when the backend surfaces `expression` on them.
  const hasExpr = $derived(
    isAssign || isComputation || ((isData || isTerminal) && node.data.expression !== undefined),
  );

  const availableVars = $derived(node.data.availableVars ?? []);
  // Derived, read-only type diagnostics for THIS node (definite type errors).
  const diagnostics = $derived(nodeDiagnostics(node));
  const hasDiag = $derived(diagnostics.length > 0);
  const diagFields = $derived(new Set(diagnostics.map((d) => diagnosticFieldHint(d.kind)).filter(Boolean)));
  const reads = $derived(data.reads ?? []);
  const moveTargets = $derived(data.getMoveTargets ? data.getMoveTargets(id) : []);
  let moveOpen = $state(false);

  const isWaitEffect = $derived(node.data.effect && WAITING_EFFECTS.has(node.data.effect));
  const terminalKind = $derived(node.data.terminalKind ?? 'finish');
  const subtitle = $derived(
    isAssign
      ? 'assignment'
      : isComputation
        ? 'sequenced compute'
        : isData
          ? 'value'
          : isTerminal
            ? 'terminal'
            : kind,
  );

  const fieldKeys = $derived(Object.keys(node.data.fields ?? {}));

  // Operation catalog entries this node may switch between (display.* for a
  // call, sleep/wait_signal for an effect), and the entry it currently matches.
  const opOptions = $derived(isInvoke ? operationsForKind(ops?.entries, kind) : []);
  const currentOpId = $derived(currentOperationId(ops?.entries, node));

  function commit() {
    // Effects are rebuilt from `data.effect` + `fields` by the lens; drop any
    // seeded raw expression so the structured path wins once the user edits.
    if (kind === 'effect') delete node.data.expression;
    data.onCommit?.();
  }

  // Switch an existing call/effect to a different operation: point the node at
  // the chosen operation and reset the arg form to that operation's typed
  // defaults. A call also takes the new entry's receiver and a freshly
  // synthesized receiver call, since the operation may belong to another
  // receiver entirely (FIG-3179); effects are rebuilt from `data.effect` +
  // fields by the lens. Either way this round-trips without delete + re-add.
  function switchOperation(event) {
    const op = (ops?.entries ?? []).find((o) => o.id === event.currentTarget.value);
    if (!op) return;
    const patch = operationSwitchPatch(kind, op);
    if (patch.operation !== undefined) node.data.operation = patch.operation;
    if (patch.receiver !== undefined) {
      if (patch.receiver === null) delete node.data.receiver;
      else node.data.receiver = patch.receiver;
    }
    if (patch.expression !== undefined) node.data.expression = patch.expression;
    if (patch.effect !== undefined) node.data.effect = patch.effect;
    if (patch.clearExpression) delete node.data.expression;
    node.data.fields = patch.fields;
    data.onCommit?.();
  }

  function setTerminalKind(next) {
    node.data.terminalKind = next;
    data.onCommit?.();
  }

  // The lens rebuilds the receiver record from `data.fields` verbatim, so both are
  // authoritative.
  let addingField = $state(false);
  let newFieldName = $state('');
  let newFieldType = $state('string');
  function addField() {
    const name = newFieldName.trim();
    const fields = node.data.fields ?? {};
    if (!name || Object.prototype.hasOwnProperty.call(fields, name)) return;
    node.data.fields = {
      ...fields,
      [name]: fieldDefaultValue({ type: newFieldType, default: newFieldType === 'number' ? 0 : '' }),
    };
    newFieldName = '';
    newFieldType = 'string';
    addingField = false;
    data.onCommit?.();
  }
  function removeField(key) {
    const next = { ...(node.data.fields ?? {}) };
    delete next[key];
    node.data.fields = next;
    data.onCommit?.();
  }

  function defaultTarget() {
    const m = (node.data.title ?? '').trim().match(/^update\s+(.+)$/i);
    return m ? m[1] : '';
  }
  function onBinding(e) {
    const v = e.currentTarget.value;
    node.data.binding = v === '' ? undefined : v;
  }
  function onTitleInput(e) {
    node.data.title = e.currentTarget.value;
    node.data.nameSource = 'label';
  }
  function onDescInput(e) {
    node.data.description = e.currentTarget.value || undefined;
    node.data.nameSource = 'label';
  }
  function onNumber(key, e) {
    const n = Number(e.currentTarget.value);
    if (!Number.isNaN(n)) node.data.fields[key] = n;
  }

  function fieldType(v) {
    if (typeof v === 'number') return 'number';
    if (typeof v === 'boolean') return 'boolean';
    if (typeof v === 'string') return 'string';
    if (v !== null && typeof v === 'object' && typeof v.$expr === 'string') return 'expr';
    return 'other';
  }
</script>

<div
  class="wf-node"
  class:is-running={status === 'running'}
  class:is-waiting={status === 'waiting'}
  class:is-done={status === 'succeeded'}
  class:is-failed={status === 'failed'}
  class:has-diag={hasDiag}
  style="--accent:{meta.accent}; width:{data.width}px; min-height:{data.height}px;"
>
  <Handle type="target" position={Position.Top} />

  <header class="wf-head">
    <span class="wf-badge" title={kind}>
      <span class="wf-glyph">{meta.glyph}</span>{meta.label}
    </span>
    {#if !isInvoke}
      <span class="wf-sub">{subtitle}</span>
    {/if}
    {#if isTerminal}
      <span class="wf-term-toggle nodrag" role="group" aria-label="Terminal kind">
        <button
          class="wf-term-btn"
          class:is-active={terminalKind === 'finish'}
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            setTerminalKind('finish');
          }}>finish</button
        >
        <button
          class="wf-term-btn wf-term-btn--fail"
          class:is-active={terminalKind === 'fail'}
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            setTerminalKind('fail');
          }}>fail</button
        >
      </span>
    {/if}
    {#if status}
      <span class="wf-status wf-status--{status}">{status}</span>
    {/if}
    {#if moveTargets.length}
      <span class="wf-move-wrap nodrag">
        <button
          class="wf-icon"
          class:is-open={moveOpen}
          title="Move to another scope"
          aria-label="Move node to another scope"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            moveOpen = !moveOpen;
          }}>⤴</button
        >
        {#if moveOpen}
          <div class="wf-move-menu">
            <div class="wf-move-label">move into</div>
            {#each moveTargets as t (t.key)}
              <button
                class="wf-move-item"
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
    {#if data.onReorder}
      <span class="wf-reorder nodrag" title="Reorder within scope">
        <button
          class="wf-move"
          title="Move earlier in execution order"
          aria-label="Move node earlier"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            data.onReorder(id, 'up');
          }}>▲</button
        >
        <button
          class="wf-move"
          title="Move later in execution order"
          aria-label="Move node later"
          onpointerdown={(e) => e.stopPropagation()}
          onclick={(e) => {
            e.stopPropagation();
            data.onReorder(id, 'down');
          }}>▼</button
        >
      </span>
    {/if}
    <button
      class="wf-del"
      title="Delete node"
      aria-label="Delete node"
      onclick={(e) => {
        e.stopPropagation();
        data.onDelete(id);
      }}>×</button
    >
  </header>

  <input
    class="wf-title"
    value={node.data.title}
    oninput={onTitleInput}
    onchange={commit}
    spellcheck="false"
    placeholder="title"
  />
  {#if node.data.nameSource === 'label' || node.data.description}
    <input
      class="wf-desc"
      value={node.data.description ?? ''}
      oninput={onDescInput}
      onchange={commit}
      spellcheck="false"
      placeholder="description…"
    />
  {/if}

  {#if reads.length}
    <div class="wf-reads" title={`reads variables from an enclosing scope: ${reads.join(', ')}`}>
      <span class="wf-reads-k">reads</span>
      {#each reads as v (v)}<span class="wf-reads-v">{v}</span>{/each}
    </div>
  {/if}

  {#if isInvoke && opOptions.length}
    <label class="wf-op nodrag">
      <span class="wf-op-k">{kind === 'call' ? 'action' : 'effect'}</span>
      <div class="wf-op-wrap">
        <select
          class="wf-op-select"
          value={currentOpId}
          onpointerdown={(e) => e.stopPropagation()}
          onchange={switchOperation}
        >
          {#if currentOpId === null}
            <option value={null} disabled>— choose —</option>
          {/if}
          {#each opOptions as op (op.id)}
            <option value={op.id}>{op.label}</option>
          {/each}
        </select>
        <span class="wf-op-caret">▾</span>
      </div>
    </label>
  {/if}

  {#if hasBinding}
    <div class="wf-bind">
      <span class="wf-bind-kw">let</span>
      <input
        class="wf-bind-input nodrag"
        value={node.data.binding ?? ''}
        oninput={onBinding}
        onchange={commit}
        spellcheck="false"
        placeholder="name (optional)"
      />
      <span class="wf-bind-eq">=</span>
    </div>
  {/if}

  {#if fieldKeys.length || kind === 'call'}
    <div class="wf-fields">
      {#each fieldKeys as key (key)}
        {@const v = node.data.fields[key]}
        {@const expected = expectedArgFieldType(node, key)}
        {@const enumOpts = enumMembers(expected)}
        {@const hint = describeExpectedType(expected)}
        <label class="wf-field" class:has-remove={kind === 'call'}>
          <span class="wf-key">{key}</span>
          {#if fieldType(v) === 'string' && enumOpts}
            <span class="wf-field-cell">
              <select
                class="wf-input wf-enum nodrag"
                value={enumMemberFromText(v, enumOpts) ?? '__custom__'}
                onpointerdown={(e) => e.stopPropagation()}
                onchange={(e) => {
                  node.data.fields[key] = e.currentTarget.value;
                  commit();
                }}
              >
                {#if enumMemberFromText(v, enumOpts) === null}
                  <option value="__custom__" disabled>{v ? v : '— choose —'}</option>
                {/if}
                {#each enumOpts as member (member)}<option value={member}>{member}</option>{/each}
              </select>
              {#if hint}<span class="wf-fieldhint">expects {hint}</span>{/if}
            </span>
          {:else if fieldType(v) === 'number'}
            <span class="wf-field-cell">
              <input
                class="wf-input wf-input--num"
                type="number"
                value={v}
                oninput={(e) => onNumber(key, e)}
                onchange={commit}
              />
              {#if hint}<span class="wf-fieldhint">expects {hint}</span>{/if}
            </span>
          {:else if fieldType(v) === 'boolean'}
            <input
              class="wf-check"
              type="checkbox"
              checked={v}
              onchange={(e) => {
                node.data.fields[key] = e.currentTarget.checked;
                commit();
              }}
            />
          {:else if fieldType(v) === 'string'}
            <span class="wf-field-cell">
              <input
                class="wf-input"
                type="text"
                value={v}
                spellcheck="false"
                oninput={(e) => (node.data.fields[key] = e.currentTarget.value)}
                onchange={commit}
              />
              {#if hint}<span class="wf-fieldhint">expects {hint}</span>{/if}
            </span>
          {:else if fieldType(v) === 'expr'}
            <ExpressionField
              value={v.$expr}
              kind="expression"
              builder="value"
              {availableVars}
              expectedType={expected}
              placeholder="expression…"
              onInput={(text) => (node.data.fields[key] = { $expr: text })}
              onCommit={commit}
            />
          {:else}
            <code class="wf-ro">{JSON.stringify(v)}</code>
          {/if}
          {#if kind === 'call'}
            <button
              class="wf-field-del"
              title="Remove argument"
              aria-label="Remove argument {key}"
              onpointerdown={(e) => e.stopPropagation()}
              onclick={(e) => {
                e.preventDefault();
                e.stopPropagation();
                removeField(key);
              }}>×</button
            >
          {/if}
        </label>
      {/each}
      {#if kind === 'call'}
        <div class="wf-addfield nodrag">
          {#if addingField}
            <input
              class="wf-af-name"
              placeholder="name"
              bind:value={newFieldName}
              spellcheck="false"
              onpointerdown={(e) => e.stopPropagation()}
              onkeydown={(e) => e.key === 'Enter' && addField()}
            />
            <select
              class="wf-af-type"
              bind:value={newFieldType}
              onpointerdown={(e) => e.stopPropagation()}
            >
              <option value="string">text</option>
              <option value="number">number</option>
              <option value="expression">expr</option>
            </select>
            <button class="wf-af-ok" title="Add argument" onclick={addField}>✓</button>
            <button class="wf-af-cancel" title="Cancel" onclick={() => (addingField = false)}>×</button
            >
          {:else}
            <button class="wf-af-open" onclick={() => (addingField = true)}>+ add field</button>
          {/if}
        </div>
      {/if}
    </div>
  {/if}

  {#if hasExpr}
    <div class="wf-expr">
      {#if isAssign}
        <div class="wf-expr-row">
          <div class="wf-expr-lhs-wrap">
            <ExpressionField
              value={node.data.target ?? defaultTarget()}
              kind="assignment_target"
              builder="target"
              {availableVars}
              placeholder="target"
              onInput={(text) => (node.data.target = text)}
              onCommit={commit}
            />
          </div>
          <span class="wf-expr-op" title="assignment">≔</span>
        </div>
      {/if}
      <ExpressionField
        value={node.data.expression ?? ''}
        kind="expression"
        builder="value"
        {availableVars}
        placeholder="expression…"
        onInput={(text) => (node.data.expression = text)}
        onCommit={commit}
      />
      <div class="wf-expr-note">
        {isAssign
          ? 'typed assignment · canonical form rebuilt on save'
          : isComputation
            ? 'typed computation · canonical form rebuilt on save'
            : 'value expression · canonical form rebuilt on save'}
      </div>
    </div>
  {/if}

  {#if isWaitEffect}
    <div class="wf-wait-note">
      {node.data.effect === 'sleep' ? 'pauses the run' : 'waits for a signal'}
    </div>
  {/if}

  {#if hasDiag}
    <div class="wf-diag" role="alert">
      {#each diagnostics as d (d.kind + d.message)}
        <div class="wf-diag-row" title={d.kind}>
          <span class="wf-diag-mark">●</span>
          <span class="wf-diag-msg">{d.message}</span>
        </div>
      {/each}
    </div>
  {/if}

  <Handle type="source" position={Position.Bottom} />
</div>

<style>
  .wf-node {
    position: relative;
    background: linear-gradient(180deg, var(--node-hi), var(--node));
    border: 1px solid var(--line);
    border-left: 3px solid var(--accent);
    border-radius: 12px;
    padding: 9px 11px 11px;
    box-shadow: var(--shadow);
    transition:
      box-shadow 0.25s ease,
      border-color 0.25s ease,
      transform 0.2s ease;
  }

  .wf-head {
    display: flex;
    align-items: center;
    gap: 7px;
    margin-bottom: 7px;
  }
  .wf-badge {
    display: inline-flex;
    align-items: center;
    gap: 4px;
    font-family: var(--font-mono);
    font-size: 9.5px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 15%, transparent);
    border: 1px solid color-mix(in srgb, var(--accent) 32%, transparent);
    padding: 2px 6px;
    border-radius: 999px;
    white-space: nowrap;
  }
  .wf-glyph {
    font-size: 11px;
    line-height: 1;
  }
  .wf-sub {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--text-faint);
    overflow: hidden;
    text-overflow: ellipsis;
    white-space: nowrap;
    flex: 1;
  }
  .wf-term-toggle {
    display: inline-flex;
    gap: 1px;
    padding: 1px;
    background: var(--ink-2);
    border: 1px solid var(--line-strong);
    border-radius: 6px;
    flex: 1;
    max-width: 130px;
  }
  .wf-term-btn {
    flex: 1;
    border: none;
    background: transparent;
    color: var(--text-faint);
    font-family: var(--font-mono);
    font-size: 8.5px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    padding: 3px 6px;
    border-radius: 5px;
    cursor: pointer;
    transition:
      color 0.12s ease,
      background 0.12s ease;
  }
  .wf-term-btn:hover {
    color: var(--text-dim);
  }
  .wf-term-btn.is-active {
    color: #041012;
    background: #5fd08a;
  }
  .wf-term-btn--fail.is-active {
    color: #180208;
    background: var(--rose);
  }
  .wf-status {
    font-family: var(--font-mono);
    font-size: 8.5px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    padding: 2px 5px;
    border-radius: 5px;
  }
  .wf-status--running {
    color: var(--cyan);
    background: color-mix(in srgb, var(--cyan) 18%, transparent);
  }
  .wf-status--waiting {
    color: var(--amber);
    background: color-mix(in srgb, var(--amber) 18%, transparent);
  }
  .wf-status--succeeded {
    color: #5fd08a;
    background: rgba(95, 208, 138, 0.16);
  }
  .wf-status--failed {
    color: var(--rose);
    background: color-mix(in srgb, var(--rose) 20%, transparent);
  }
  .wf-del {
    border: none;
    background: transparent;
    color: var(--text-faint);
    font-size: 17px;
    line-height: 1;
    width: 20px;
    height: 20px;
    border-radius: 6px;
    padding: 0;
    transition:
      color 0.15s ease,
      background 0.15s ease;
  }
  .wf-del:hover {
    color: var(--rose);
    background: color-mix(in srgb, var(--rose) 16%, transparent);
  }

  .wf-move-wrap {
    position: relative;
    flex-shrink: 0;
  }
  .wf-icon {
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
  .wf-icon:hover,
  .wf-icon.is-open {
    color: var(--cyan);
    background: color-mix(in srgb, var(--cyan) 16%, transparent);
  }
  .wf-move-menu {
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
  .wf-move-label {
    font-family: var(--font-mono);
    font-size: 8px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--text-faint);
    padding: 2px 6px 4px;
  }
  .wf-move-item {
    text-align: left;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 6px;
    color: var(--text-dim);
    font-family: var(--font-mono);
    font-size: 10.5px;
    padding: 4px 7px;
    cursor: pointer;
    transition:
      color 0.12s ease,
      background 0.12s ease,
      border-color 0.12s ease;
  }
  .wf-move-item:hover {
    color: var(--text);
    background: color-mix(in srgb, var(--cyan) 12%, transparent);
    border-color: color-mix(in srgb, var(--cyan) 40%, transparent);
  }

  .wf-reorder {
    display: inline-flex;
    flex-direction: column;
    gap: 1px;
    flex-shrink: 0;
  }
  .wf-move {
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
  .wf-move:hover {
    color: var(--accent);
    background: color-mix(in srgb, var(--accent) 16%, transparent);
  }

  .wf-title {
    width: 100%;
    background: transparent;
    border: none;
    border-bottom: 1px dashed transparent;
    color: var(--text);
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 14px;
    padding: 1px 0 3px;
    margin-bottom: 2px;
  }
  .wf-title:focus,
  .wf-desc:focus {
    outline: none;
    border-bottom-color: var(--accent);
  }
  .wf-desc {
    width: 100%;
    background: transparent;
    border: none;
    border-bottom: 1px dashed transparent;
    color: var(--text-dim);
    font-family: var(--font-ui);
    font-size: 11.5px;
    padding: 1px 0 4px;
    margin-bottom: 4px;
  }

  .wf-op {
    display: flex;
    align-items: center;
    gap: 7px;
    margin: 2px 0 6px;
  }
  .wf-op-k {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--text-faint);
    flex-shrink: 0;
  }
  .wf-op-wrap {
    position: relative;
    flex: 1;
    min-width: 0;
  }
  .wf-op-select {
    appearance: none;
    width: 100%;
    background: color-mix(in srgb, var(--accent) 8%, var(--ink-2));
    border: 1px solid color-mix(in srgb, var(--accent) 34%, var(--line));
    border-radius: 7px;
    color: var(--text);
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 12px;
    padding: 5px 26px 5px 9px;
    cursor: pointer;
    transition: border-color 0.15s ease;
  }
  .wf-op-select:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 14%, transparent);
  }
  .wf-op-select option {
    background: var(--ink-2);
    color: var(--text);
  }
  .wf-op-caret {
    position: absolute;
    right: 9px;
    top: 50%;
    transform: translateY(-50%);
    color: var(--accent);
    font-size: 9px;
    pointer-events: none;
  }

  .wf-reads {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 5px;
    margin: 2px 0 6px;
    padding: 4px 7px;
    background: color-mix(in srgb, var(--cyan) 7%, transparent);
    border: 1px dashed color-mix(in srgb, var(--cyan) 30%, var(--line));
    border-radius: 7px;
  }
  .wf-reads-k {
    font-family: var(--font-mono);
    font-size: 8.5px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: color-mix(in srgb, var(--cyan) 80%, var(--text-dim));
  }
  .wf-reads-v {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--cyan);
    background: color-mix(in srgb, var(--cyan) 12%, transparent);
    border-radius: 5px;
    padding: 1px 6px;
  }

  .wf-bind {
    display: flex;
    align-items: center;
    gap: 7px;
    margin-top: 2px;
    margin-bottom: 4px;
  }
  .wf-bind-kw {
    font-family: var(--font-mono);
    font-size: 11px;
    letter-spacing: 0.04em;
    color: var(--text-faint);
    flex-shrink: 0;
  }
  .wf-bind-eq {
    font-family: var(--font-mono);
    font-size: 13px;
    color: var(--accent);
    flex-shrink: 0;
  }
  .wf-bind-input {
    flex: 1;
    min-width: 0;
    background: var(--ink-2);
    border: 1px solid var(--line);
    border-radius: 7px;
    color: var(--text-dim);
    font-family: var(--font-mono);
    font-size: 11.5px;
    font-weight: 500;
    padding: 5px 8px;
    transition: border-color 0.15s ease;
  }
  .wf-bind-input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 14%, transparent);
  }

  .wf-fields {
    display: flex;
    flex-direction: column;
    gap: 6px;
    margin-top: 4px;
  }
  .wf-field {
    display: grid;
    grid-template-columns: 62px 1fr;
    align-items: center;
    gap: 8px;
  }
  .wf-field.has-remove {
    grid-template-columns: 58px 1fr auto;
  }
  .wf-field-del {
    border: none;
    background: transparent;
    color: var(--text-faint);
    font-size: 14px;
    line-height: 1;
    width: 18px;
    height: 18px;
    border-radius: 5px;
    padding: 0;
    cursor: pointer;
    transition:
      color 0.12s ease,
      background 0.12s ease;
  }
  .wf-field-del:hover {
    color: var(--rose);
    background: color-mix(in srgb, var(--rose) 16%, transparent);
  }
  .wf-addfield {
    display: flex;
    align-items: center;
    gap: 5px;
    margin-top: 2px;
  }
  .wf-af-open {
    font-family: var(--font-mono);
    font-size: 9.5px;
    letter-spacing: 0.04em;
    color: var(--text-faint);
    background: transparent;
    border: 1px dashed var(--line-strong);
    border-radius: 6px;
    padding: 4px 9px;
    cursor: pointer;
    transition:
      color 0.12s ease,
      border-color 0.12s ease;
  }
  .wf-af-open:hover {
    color: var(--accent);
    border-color: color-mix(in srgb, var(--accent) 50%, var(--line-strong));
  }
  .wf-af-name {
    flex: 1;
    min-width: 0;
    background: var(--ink-2);
    border: 1px solid var(--line);
    border-radius: 6px;
    color: var(--text);
    font-family: var(--font-mono);
    font-size: 11px;
    padding: 4px 7px;
  }
  .wf-af-name:focus {
    outline: none;
    border-color: var(--accent);
  }
  .wf-af-type {
    background: var(--ink-2);
    border: 1px solid var(--line);
    border-radius: 6px;
    color: var(--text-dim);
    font-family: var(--font-mono);
    font-size: 10px;
    padding: 4px 5px;
    cursor: pointer;
  }
  .wf-af-ok,
  .wf-af-cancel {
    border: none;
    background: transparent;
    font-size: 13px;
    line-height: 1;
    width: 20px;
    height: 20px;
    border-radius: 5px;
    padding: 0;
    cursor: pointer;
  }
  .wf-af-ok {
    color: #5fd08a;
  }
  .wf-af-ok:hover {
    background: rgba(95, 208, 138, 0.16);
  }
  .wf-af-cancel {
    color: var(--text-faint);
  }
  .wf-af-cancel:hover {
    color: var(--rose);
    background: color-mix(in srgb, var(--rose) 16%, transparent);
  }
  .wf-key {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--text-faint);
    text-align: right;
    overflow: hidden;
    text-overflow: ellipsis;
  }
  .wf-input {
    width: 100%;
    background: var(--ink-2);
    border: 1px solid var(--line);
    border-radius: 7px;
    color: var(--text);
    font-family: var(--font-mono);
    font-size: 11.5px;
    padding: 5px 8px;
    transition: border-color 0.15s ease;
  }
  .wf-input:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 14%, transparent);
  }
  .wf-input--num {
    font-variant-numeric: tabular-nums;
  }
  .wf-check {
    justify-self: start;
    accent-color: var(--accent);
    width: 15px;
    height: 15px;
  }
  .wf-ro {
    font-family: var(--font-mono);
    font-size: 10.5px;
    color: var(--text-dim);
  }
  .wf-field-cell {
    display: flex;
    flex-direction: column;
    gap: 2px;
    min-width: 0;
  }
  .wf-enum {
    appearance: none;
    cursor: pointer;
  }
  .wf-fieldhint {
    font-family: var(--font-mono);
    font-size: 8px;
    letter-spacing: 0.03em;
    color: var(--text-faint);
  }

  /* inline type diagnostics — definite errors surfaced on the node */
  .wf-diag {
    margin-top: 8px;
    display: flex;
    flex-direction: column;
    gap: 4px;
    padding: 6px 8px;
    background: color-mix(in srgb, var(--rose) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--rose) 40%, transparent);
    border-radius: 8px;
  }
  .wf-diag-row {
    display: flex;
    align-items: flex-start;
    gap: 6px;
  }
  .wf-diag-mark {
    color: var(--rose);
    font-size: 8px;
    line-height: 1.6;
    flex-shrink: 0;
  }
  .wf-diag-msg {
    font-family: var(--font-mono);
    font-size: 9.5px;
    line-height: 1.4;
    color: #ffb4c6;
  }
  .wf-wait-note {
    margin-top: 8px;
    font-family: var(--font-mono);
    font-size: 9.5px;
    color: var(--amber);
    opacity: 0.8;
    letter-spacing: 0.04em;
  }

  .wf-expr {
    margin-top: 6px;
    display: flex;
    flex-direction: column;
    gap: 5px;
  }
  .wf-expr-row {
    display: flex;
    align-items: center;
    gap: 7px;
  }
  .wf-expr-lhs-wrap {
    flex: 1;
    min-width: 0;
    display: flex;
  }
  .wf-expr-op {
    font-family: var(--font-mono);
    font-size: 14px;
    color: var(--accent);
    flex-shrink: 0;
  }
  .wf-expr-note {
    font-family: var(--font-mono);
    font-size: 9px;
    color: var(--text-faint);
    letter-spacing: 0.03em;
  }

  /* definite type error — red left rail + ring so the node reads as broken */
  .wf-node.has-diag {
    border-color: color-mix(in srgb, var(--rose) 55%, var(--line));
    border-left-color: var(--rose);
    box-shadow:
      0 0 0 1px color-mix(in srgb, var(--rose) 35%, transparent),
      var(--shadow);
  }

  .wf-node.is-running {
    border-color: var(--cyan);
    box-shadow:
      0 0 0 1px var(--cyan),
      0 0 26px -4px color-mix(in srgb, var(--cyan) 65%, transparent),
      var(--shadow);
  }
  .wf-node.is-waiting {
    border-color: var(--amber);
    animation: wf-pulse 1.1s ease-in-out infinite;
  }
  .wf-node.is-done {
    border-left-color: #5fd08a;
  }
  .wf-node.is-failed {
    border-color: var(--rose);
    box-shadow:
      0 0 0 1px var(--rose),
      0 0 26px -6px color-mix(in srgb, var(--rose) 60%, transparent),
      var(--shadow);
  }
  @keyframes wf-pulse {
    0%,
    100% {
      box-shadow:
        0 0 0 1px color-mix(in srgb, var(--amber) 55%, transparent),
        0 0 8px -2px color-mix(in srgb, var(--amber) 30%, transparent),
        var(--shadow);
    }
    50% {
      box-shadow:
        0 0 0 1px var(--amber),
        0 0 28px -2px color-mix(in srgb, var(--amber) 75%, transparent),
        var(--shadow);
    }
  }
</style>
