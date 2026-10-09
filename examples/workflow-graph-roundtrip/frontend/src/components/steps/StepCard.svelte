<script>
  import { getContext } from 'svelte';
  import StepCard from './StepCard.svelte';
  import AddStepButton from './AddStepButton.svelte';
  import EditableValue from './EditableValue.svelte';
  import ArgField from './ArgField.svelte';
  import ToolIcon from './ToolIcon.svelte';
  import ValueToken from './ValueToken.svelte';
  import IdentifierField from '../IdentifierField.svelte';
  import { stepLabel, rowDiagnostics, stepSummary, stepTag } from '../../lib/steps.js';
  import { kindMeta } from '../../lib/nodeKinds.js';

  // One workflow step as a big Zapier-style CARD (item 1). A card is a header
  // (tool/app icon + step tag + editable human title + app name + status) plus,
  // for a configurable action, a body that is COLLAPSED by default (item 3): the
  // collapsed card shows a one-line human summary of its key args; expanding it
  // reveals the labeled config fields. Branch/loop cards are CONTAINER cards that
  // visually hold their nested child-cards under a plain-language header (the one
  // structural rule Zapier lacks). Recurses through <StepCard> for nesting.
  // `primary` marks a card as part of the primary top-to-bottom flow (the main
  // list, or a process body): its first step reads as the "Trigger" and the rest
  // as numbered "Action N". Steps nested inside a branch/loop are not primary —
  // they read as "Step N" within that block.
  let { entry, catalog = [], index = 0, depth = 0, primary = false } = $props();

  const handlers = getContext('stepsHandlers') ?? {};
  const node = $derived(entry.node);
  const label = $derived(stepLabel(node, catalog));
  const diags = $derived(rowDiagnostics(node));
  const availableVars = $derived(node.data.availableVars ?? []);
  const accent = $derived(kindMeta(node.data.kind).accent);

  const fieldKeys = $derived(Object.keys(node.data.fields ?? {}));
  const isAction = $derived(label.category === 'action');
  const isDataConsumer = $derived(isAction && (label.icon === 'agent' || label.icon === 'llm'));
  // An action with configurable args collapses; everything else is already a
  // single calm line and stays open.
  const expandable = $derived(isAction && !label.minor && fieldKeys.length > 0);
  const summary = $derived(stepSummary(node));
  const displayTitle = $derived(
    node.data.nameSource === 'label' ? (node.data.title ?? '') : label.name,
  );

  // A container/process carries nested body slot(s) directly under its own line.
  const isContainerLike = $derived(
    ['if', 'while', 'for', 'comprehension', 'loop', 'process', 'try', 'scope'].includes(
      label.category,
    ),
  );
  const clauses = $derived(node.data.clauses ?? []);

  // Step tag: the first step of a primary flow reads as the Trigger, the rest as
  // numbered Actions; steps inside a branch/loop are numbered as plain Steps.
  const tag = $derived(stepTag(primary, index));
  const isTrigger = $derived(tag.role === 'trigger');
  // The Trigger names its source (the integration that starts the flow) — the
  // action's tool/app name when it is an action, else nothing extra.
  const triggerSource = $derived(isTrigger && isAction ? label.toolName : null);
  // A process body IS a primary flow; a branch/loop body is not.
  const childPrimary = $derived(label.category === 'process');

  // Collapsed by default for expandable action cards.
  let collapsed = $state(true);
  // A card that carries a diagnostic auto-opens so the problem is visible.
  $effect(() => {
    if (diags.length) collapsed = false;
  });

  function commit() {
    handlers.onCommit?.();
  }
  function del() {
    handlers.onDelete?.(entry.id);
  }
  function insertInto(slot, at, operation) {
    handlers.onInsert?.({ ownerId: entry.id, slot }, at, operation);
  }
</script>

{#snippet tagBadge(muted)}
  {#if isTrigger}
    <span class="tag tag--trigger"><span class="trigger-bolt" aria-hidden="true">⚡</span>Trigger</span>
    {#if triggerSource}<span class="trigger-src">{triggerSource}</span>{/if}
  {:else}
    <span class="tag" class:tag--muted={muted}>{tag.text}</span>
  {/if}
{/snippet}

<div
  class="card"
  class:is-action={isAction}
  class:is-branch={isContainerLike}
  class:is-minor={label.minor}
  class:has-diag={diags.length}
  class:is-collapsed={expandable && collapsed}
  class:is-trigger={isTrigger}
  style="--accent:{accent}"
>
  <!-- The card header. An expandable action card is opened/closed via the
       chevron button or by clicking its collapsed summary line (both below). -->
  <div class="head">
    {#if isAction && !label.minor}
      <span class="icon icon--{label.icon}" title={label.toolName}>
        <ToolIcon kind={label.icon} />
      </span>
    {:else}
      <span class="glyph" style="color:{accent}">{label.glyph}</span>
    {/if}

    <div class="head-main">
      {#if isAction}
        <!-- Action card: tag + app-name meta row, then editable human title.
             The Trigger already names its source, so the separate app pill is
             dropped for the trigger. -->
        <div class="meta">
          {@render tagBadge(false)}
          {#if !isTrigger}<span class="app">{label.toolName}</span>{/if}
        </div>
        <input
          class="title-in"
          value={displayTitle}
          aria-label="Step title"
          oninput={(e) => {
            node.data.title = e.currentTarget.value;
            node.data.nameSource = 'label';
          }}
          onchange={commit}
          spellcheck="false"
          placeholder="what this does"
        />
        {#if expandable && collapsed}
          <button
            class="summary"
            type="button"
            title="Open settings"
            onclick={() => (collapsed = false)}
          >
            {#if isDataConsumer && entry.inputs.length}
              <span class="summary-using">using</span>
              {#each entry.inputs as input, i (input.name)}
                {#if i > 0}<span class="summary-sep">·</span>{/if}
                <ValueToken desc={{ kind: 'token', name: input.name, varType: input.type }} />
              {/each}
            {:else if summary}
              <span class="summary-text">{summary}</span>
            {:else}
              <span class="summary-empty">No settings yet — click to configure</span>
            {/if}
          </button>
        {/if}
      {:else}
        <!-- Line card: the plain-language editors read directly under the icon.
             Structural cards (branch/loop/process) skip the numeric tag — their
             plain-language line already names them. -->
        {#if !isContainerLike && !label.minor}
          <div class="meta">{@render tagBadge(true)}</div>
        {/if}
        <div class="line">
          {#if label.category === 'save'}
            <span class="lead">Save</span>
            <span class="name-slot">
              <IdentifierField
                value={node.data.binding ?? ''}
                variant="box"
                placeholder="name"
                ariaLabel="Saved name"
                onInput={(v) => (node.data.binding = v === '' ? undefined : v)}
                onCommit={commit}
              />
            </span>
            <span class="lead">as</span>
            <EditableValue
              value={node.data.expression ?? ''}
              {availableVars}
              builder="value"
              placeholder="a value"
              onInput={(t) => (node.data.expression = t)}
              onCommit={commit}
            />
          {:else if label.category === 'set'}
            <span class="lead">Set</span>
            <EditableValue
              value={node.data.target ?? ''}
              {availableVars}
              builder="target"
              placeholder="what to set"
              onInput={(t) => (node.data.target = t)}
              onCommit={commit}
            />
            <span class="lead">to</span>
            <EditableValue
              value={node.data.expression ?? ''}
              {availableVars}
              builder="value"
              placeholder="a value"
              onInput={(t) => (node.data.expression = t)}
              onCommit={commit}
            />
          {:else if label.category === 'finish' || label.category === 'fail'}
            <span class="lead">{label.lead}</span>
            <EditableValue
              value={node.data.expression ?? ''}
              {availableVars}
              builder="value"
              placeholder="a result"
              onInput={(t) => (node.data.expression = t)}
              onCommit={commit}
            />
          {:else if label.category === 'if' || label.category === 'while'}
            <span class="lead">{label.lead}</span>
            <EditableValue
              value={node.data.condition ?? ''}
              {availableVars}
              builder="comparison"
              placeholder="a condition"
              onInput={(t) => (node.data.condition = t)}
              onCommit={commit}
            />
            <span class="lead">:</span>
          {:else if label.category === 'for'}
            <span class="lead">For each</span>
            <span class="name-slot">
              <IdentifierField
                value={node.data.binding ?? ''}
                variant="box"
                placeholder="item"
                ariaLabel="Each item name"
                onInput={(v) => (node.data.binding = v)}
                onCommit={commit}
              />
            </span>
            <span class="lead">in</span>
            <EditableValue
              value={node.data.iterable ?? ''}
              {availableVars}
              builder="list"
              expectedType="list[any]"
              placeholder="a list"
              onInput={(t) => (node.data.iterable = t)}
              onCommit={commit}
            />
            <span class="lead">:</span>
          {:else if label.category === 'comprehension'}
            <span class="lead">Make a list — for each</span>
            <span class="name-slot">
              <IdentifierField
                value={node.data.binding ?? ''}
                variant="box"
                placeholder="item"
                ariaLabel="Comprehension name"
                onInput={(v) => (node.data.binding = v === '' ? undefined : v)}
                onCommit={commit}
              />
            </span>
            {#each clauses as clause, i (i)}
              {#if clause.kind === 'for'}
                <span class="lead">over</span>
                <EditableValue
                  value={clause.iterable ?? ''}
                  {availableVars}
                  builder="list"
                  expectedType="list[any]"
                  placeholder="a list"
                  onInput={(t) => (node.data.clauses[i].iterable = t)}
                  onCommit={commit}
                />
              {:else}
                <span class="lead">keeping only</span>
                <EditableValue
                  value={clause.condition ?? ''}
                  {availableVars}
                  builder="comparison"
                  placeholder="a condition"
                  onInput={(t) => (node.data.clauses[i].condition = t)}
                  onCommit={commit}
                />
              {/if}
            {/each}
            <span class="lead">:</span>
          {:else if label.category === 'process'}
            <span class="lead">Background task</span>
            <span class="name-slot name-slot--wide">
              <IdentifierField
                value={node.data.name ?? node.data.title ?? ''}
                variant="box"
                placeholder="task_name"
                ariaLabel="Task name"
                onInput={(v) => {
                  node.data.name = v;
                  node.data.title = v;
                }}
                onCommit={commit}
              />
            </span>
          {:else if label.category === 'sleep'}
            <span class="lead">Wait for</span>
            {#if fieldKeys.includes('duration')}
              <ArgField {node} fieldKey="duration" onCommit={commit} />
            {:else}
              <span class="name">{label.name}</span>
            {/if}
          {:else if label.category === 'start' || label.category === 'await' || label.category === 'effect'}
            <span class="lead">{label.lead}</span>
            {#if label.name}<span class="name">{label.name}</span>{/if}
            {#if label.tail}<span class="lead">{label.tail}</span>{/if}
          {:else if label.category === 'throw'}
            <span class="lead">Throw</span>
            <code class="thrown">{node.data.expression ?? ''}</code>
          {:else}
            <span class="lead">{label.lead}</span>
            {#if label.name}<span class="name">{label.name}</span>{/if}
          {/if}
        </div>
      {/if}
    </div>

    <div class="head-side">
      {#if !label.minor}
        {#if diags.length}
          <span class="status status--warn" title="This step needs attention">
            <span class="status-ico">▲</span>{diags.length}
          </span>
        {:else}
          <span class="status status--ok" title="This step looks good" aria-label="Valid">✓</span>
        {/if}
      {/if}
      {#if expandable}
        <button
          class="chev"
          type="button"
          aria-expanded={!collapsed}
          aria-label={collapsed ? 'Open settings' : 'Close settings'}
          onclick={(e) => {
            e.stopPropagation();
            collapsed = !collapsed;
          }}>{collapsed ? '▸' : '▾'}</button
        >
      {/if}
      {#if handlers.onDelete}
        <!-- svelte-ignore a11y_click_events_have_key_events -->
        <button
          class="del"
          type="button"
          title="Remove this step"
          aria-label="Remove step"
          onclick={(e) => {
            e.stopPropagation();
            del();
          }}>×</button
        >
      {/if}
    </div>
  </div>

  <!-- Expanded action config -->
  {#if isAction && fieldKeys.length && !(expandable && collapsed)}
    <div class="body">
      {#if isDataConsumer && entry.inputs.length}
        <div class="using">
          <span class="using-key">using</span>
          <span class="using-vals">
            {#each entry.inputs as input, i (input.name)}
              {#if i > 0}<span class="using-and">and</span>{/if}
              <ValueToken desc={{ kind: 'token', name: input.name, varType: input.type }} />
            {/each}
          </span>
        </div>
      {/if}
      {#if node.data.binding}
        <div class="field">
          <span class="field-key">save as</span>
          <span class="name-slot">
            <IdentifierField
              value={node.data.binding}
              variant="box"
              placeholder="name"
              ariaLabel="Saved name"
              onInput={(v) => (node.data.binding = v === '' ? undefined : v)}
              onCommit={commit}
            />
          </span>
        </div>
      {/if}
      {#each fieldKeys as key (key)}
        <div class="field"><ArgField {node} fieldKey={key} onCommit={commit} /></div>
      {/each}
    </div>
  {/if}

  <!-- Friendly diagnostics -->
  {#if diags.length}
    <div class="diags" role="alert">
      {#each diags as d (d.kind + d.message)}
        <div class="diag"><span class="diag-mark">▲</span>{d.friendly}</div>
      {/each}
    </div>
  {/if}
</div>

<!-- Nested body: a container/process visually HOLDS its child cards, indented
     under a plain-language sub-header, with the rail + "+" continuing inside. -->
{#if isContainerLike && entry.groups.length}
  {#each entry.groups as group (group.slot)}
    <div class="nest">
      {#if group.header}
        <div class="group-header">
          <span class="glyph" style="color:{accent}">{label.glyph}</span>{group.header}:
        </div>
      {/if}
      <div class="nest-body" style="--accent:{accent}">
        {#if handlers.onInsert}
          <AddStepButton
            {catalog}
            topLevel={false}
            label="Add a step here"
            onPick={(op) => insertInto(group.slot, 0, op)}
          />
        {/if}
        {#each group.steps as child, ci (child.id)}
          <StepCard entry={child} {catalog} index={ci} depth={depth + 1} primary={childPrimary} />
          {#if handlers.onInsert}
            <AddStepButton
              {catalog}
              topLevel={false}
              label="Add a step here"
              onPick={(op) => insertInto(group.slot, ci + 1, op)}
            />
          {/if}
        {/each}
        {#if !group.steps.length}
          <div class="nest-empty">Nothing here yet.</div>
        {/if}
      </div>
    </div>
  {/each}
{/if}

<style>
  .card {
    position: relative;
    background: color-mix(in srgb, var(--ink-2) 88%, transparent);
    border: 1px solid var(--line);
    border-radius: 14px;
    box-shadow: 0 2px 10px -6px rgba(0, 0, 0, 0.5);
    transition:
      border-color 0.16s ease,
      box-shadow 0.16s ease,
      background 0.16s ease;
  }
  .card:hover {
    border-color: var(--line-strong);
  }
  .card.is-action {
    border-left: 3px solid color-mix(in srgb, var(--accent) 70%, transparent);
  }
  /* The trigger card leads the flow: a warm accent edge + soft glow set it
     apart from the actions that follow. */
  .card.is-trigger {
    border-left: 3px solid var(--amber);
    background: color-mix(in srgb, var(--amber) 7%, var(--ink-2));
    box-shadow:
      0 2px 10px -6px rgba(0, 0, 0, 0.5),
      0 0 0 1px color-mix(in srgb, var(--amber) 22%, transparent);
  }
  .card.is-trigger:hover {
    border-color: color-mix(in srgb, var(--amber) 40%, var(--line-strong));
  }
  .card.is-branch {
    border-left: 3px solid color-mix(in srgb, var(--accent) 70%, transparent);
    background: color-mix(in srgb, var(--accent) 6%, var(--ink-2));
  }
  .card.has-diag {
    border-color: color-mix(in srgb, var(--rose) 45%, var(--line-strong));
  }
  /* Display-feedback calls (set_status/progress/light/highlight) are UI side
     effects, not user-meaningful steps — rendered slim + muted. */
  .card.is-minor {
    opacity: 0.5;
    box-shadow: none;
  }
  .card.is-minor:hover {
    opacity: 0.8;
  }

  .head {
    display: flex;
    align-items: flex-start;
    gap: 13px;
    padding: 13px 14px;
  }
  .is-minor .head {
    padding: 7px 12px;
    gap: 10px;
    align-items: center;
  }

  .icon {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 40px;
    height: 40px;
    flex: 0 0 40px;
    border-radius: 11px;
    color: var(--text-dim);
    background: var(--ink-3);
    border: 1px solid var(--line-strong);
  }
  .icon--slack {
    color: var(--rose);
  }
  .icon--github {
    color: var(--text);
  }
  .icon--email {
    color: var(--amber);
  }
  .icon--web {
    color: var(--cyan);
  }
  .icon--llm,
  .icon--agent {
    color: var(--violet);
  }
  .icon--message {
    color: var(--cyan);
  }
  .glyph {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 28px;
    height: 28px;
    flex: 0 0 28px;
    margin-top: 1px;
    font-size: 15px;
    border-radius: 9px;
    background: color-mix(in srgb, var(--accent) 12%, transparent);
    border: 1px solid color-mix(in srgb, var(--accent) 30%, var(--line));
  }
  .is-minor .glyph {
    width: 20px;
    height: 20px;
    flex-basis: 20px;
    font-size: 12px;
    background: transparent;
    border: none;
  }

  .head-main {
    flex: 1;
    min-width: 0;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .meta {
    display: flex;
    align-items: center;
    gap: 8px;
  }
  .tag {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.12em;
    text-transform: uppercase;
    color: var(--cyan);
    background: color-mix(in srgb, var(--cyan) 12%, transparent);
    border-radius: 5px;
    padding: 2px 6px;
  }
  .tag--muted {
    color: var(--text-faint);
    background: transparent;
    padding: 0;
  }
  /* The Trigger badge: a distinct, warm, high-contrast chip so a non-technical
     reader instantly sees "this is what starts the workflow". */
  .tag--trigger {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    color: var(--ink);
    background: linear-gradient(180deg, var(--amber), #e0a93c);
    box-shadow: 0 2px 8px -3px color-mix(in srgb, var(--amber) 70%, transparent);
    letter-spacing: 0.1em;
  }
  .trigger-bolt {
    font-size: 9px;
    line-height: 1;
  }
  .trigger-src {
    font-family: var(--font-ui);
    font-size: 12px;
    font-weight: 700;
    color: var(--text);
  }
  .trigger-src::before {
    content: '·';
    margin-right: 5px;
    color: var(--text-faint);
    font-weight: 400;
  }
  .app {
    font-family: var(--font-mono);
    font-size: 9px;
    letter-spacing: 0.05em;
    color: var(--text-faint);
    text-transform: uppercase;
  }
  .title-in {
    width: 100%;
    background: transparent;
    border: none;
    border-bottom: 1px solid transparent;
    color: var(--text);
    font-family: var(--font-ui);
    font-weight: 650;
    font-size: 15px;
    padding: 1px 0;
  }
  .title-in:hover {
    border-bottom-color: var(--line-strong);
  }
  .title-in:focus {
    outline: none;
    border-bottom-color: var(--accent);
  }

  .summary {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 5px 7px;
    margin-top: 2px;
    padding: 3px 6px;
    margin-left: -6px;
    width: fit-content;
    max-width: 100%;
    font-size: 12.5px;
    color: var(--text-dim);
    background: transparent;
    border: 1px solid transparent;
    border-radius: 8px;
    cursor: pointer;
    text-align: left;
    transition:
      background 0.12s ease,
      border-color 0.12s ease;
  }
  .summary:hover {
    background: color-mix(in srgb, var(--cyan) 7%, transparent);
    border-color: color-mix(in srgb, var(--cyan) 26%, var(--line));
  }
  .summary-using {
    color: var(--text-faint);
    font-size: 11px;
    text-transform: uppercase;
    letter-spacing: 0.05em;
  }
  .summary-sep {
    color: var(--text-faint);
  }
  .summary-text {
    color: var(--text-dim);
  }
  .summary-empty {
    color: var(--text-faint);
    font-style: italic;
  }

  .line {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px 8px;
    font-size: 14px;
    line-height: 1.55;
    color: var(--text);
  }
  .lead {
    color: var(--text-dim);
  }
  .name {
    font-weight: 600;
    color: var(--text);
  }
  .name-slot {
    display: inline-flex;
    min-width: 70px;
    max-width: 150px;
  }
  .name-slot--wide {
    max-width: 240px;
  }
  .thrown {
    font-family: var(--font-mono);
    font-size: 11.5px;
    color: var(--text-dim);
    background: var(--ink-2);
    border: 1px solid var(--line);
    border-radius: 6px;
    padding: 2px 7px;
  }

  .head-side {
    display: flex;
    align-items: center;
    gap: 8px;
    flex-shrink: 0;
    padding-top: 2px;
  }
  .status {
    display: inline-flex;
    align-items: center;
    gap: 3px;
    font-size: 11px;
    font-weight: 700;
    border-radius: 999px;
    line-height: 1;
  }
  .status--ok {
    width: 20px;
    height: 20px;
    justify-content: center;
    color: #5fd08a;
    background: color-mix(in srgb, #5fd08a 14%, transparent);
    border: 1px solid color-mix(in srgb, #5fd08a 34%, transparent);
  }
  .status--warn {
    padding: 3px 8px;
    color: var(--amber);
    background: color-mix(in srgb, var(--amber) 14%, transparent);
    border: 1px solid color-mix(in srgb, var(--amber) 40%, transparent);
  }
  .status-ico {
    font-size: 8px;
  }
  .chev {
    background: transparent;
    border: none;
    color: var(--text-faint);
    font-size: 11px;
    width: 18px;
    height: 18px;
    padding: 0;
    border-radius: 5px;
    cursor: pointer;
    transition: color 0.12s ease, background 0.12s ease;
  }
  .chev:hover {
    color: var(--text);
    background: var(--ink-3);
  }
  .del {
    background: transparent;
    border: none;
    color: var(--text-faint);
    font-size: 17px;
    line-height: 1;
    width: 24px;
    height: 24px;
    border-radius: 7px;
    opacity: 0;
    transition:
      opacity 0.12s ease,
      color 0.12s ease,
      background 0.12s ease;
  }
  .card:hover .del {
    opacity: 1;
  }
  .del:hover {
    color: var(--rose);
    background: color-mix(in srgb, var(--rose) 15%, transparent);
  }

  .body {
    padding: 4px 15px 13px 67px;
    display: flex;
    flex-direction: column;
    gap: 7px;
  }
  .field {
    display: flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .field-key {
    font-family: var(--font-mono);
    font-size: 10px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--text-faint);
    min-width: 54px;
    text-align: right;
    flex-shrink: 0;
  }
  .using {
    display: flex;
    align-items: center;
    gap: 10px;
    min-width: 0;
    padding-bottom: 8px;
    margin-bottom: 2px;
    border-bottom: 1px solid var(--line);
  }
  .using-key {
    min-width: 54px;
    flex-shrink: 0;
    text-align: right;
    color: var(--text-dim);
    font-size: 12px;
    font-weight: 600;
  }
  .using-vals {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 6px;
    min-width: 0;
  }
  .using-and {
    color: var(--text-faint);
    font-size: 11px;
  }

  .diags {
    padding: 0 15px 12px 67px;
    display: flex;
    flex-direction: column;
    gap: 3px;
  }
  .is-branch > .diags {
    padding-left: 55px;
  }
  .diag {
    display: flex;
    align-items: flex-start;
    gap: 7px;
    font-size: 12.5px;
    line-height: 1.4;
    color: #ffb4c6;
  }
  .diag-mark {
    color: var(--rose);
    font-size: 9px;
    line-height: 1.55;
    flex-shrink: 0;
  }

  /* --- nesting (container body) --- */
  .nest {
    margin-left: 22px;
    padding-left: 20px;
    border-left: 2px solid color-mix(in srgb, var(--accent) 34%, var(--line));
  }
  .group-header {
    display: flex;
    align-items: center;
    gap: 9px;
    padding: 8px 2px 2px;
    font-size: 13.5px;
    font-weight: 600;
    color: var(--text-dim);
  }
  .nest-body {
    display: flex;
    flex-direction: column;
  }
  .nest-empty {
    font-size: 12px;
    color: var(--text-faint);
    font-style: italic;
    padding: 4px 2px 8px;
  }
</style>
