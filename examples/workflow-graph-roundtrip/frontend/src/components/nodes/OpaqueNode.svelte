<script>
  import { Handle, Position } from '@xyflow/svelte';
  import { getContext } from 'svelte';
  import { kindMeta } from '../../lib/nodeKinds.js';

  let { id, data } = $props();

  const run = getContext('run');
  const mode = getContext('mode');
  const node = $derived(data.node);
  const meta = kindMeta('opaque');
  const status = $derived(run.overlay[id] ?? null);
  const rows = $derived(Math.min(Math.max((node.data.source ?? '').split('\n').length, 3), 16));
</script>

<div
  class="wf-node opaque"
  class:is-running={status === 'running'}
  class:is-waiting={status === 'waiting'}
  class:is-done={status === 'succeeded'}
  class:is-failed={status === 'failed'}
  style="--accent:{meta.accent}; width:{data.width}px;"
>
  <Handle type="target" position={Position.Top} />
  <header class="wf-head">
    <span class="wf-badge"><span class="wf-glyph">{meta.glyph}</span>opaque</span>
    <span class="wf-sub">edit as text</span>
    {#if status}<span class="wf-status wf-status--{status}">{status}</span>{/if}
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
    class="wf-title nodrag"
    value={node.data.title}
    oninput={(e) => {
      node.data.title = e.currentTarget.value;
      node.data.nameSource = 'label';
    }}
    onchange={() => data.onCommit?.()}
    spellcheck="false"
    placeholder="title"
  />
  {#if mode.power}
    <textarea
      class="opaque-src nodrag"
      spellcheck="false"
      {rows}
      value={node.data.source ?? ''}
      oninput={(e) => (node.data.source = e.currentTarget.value)}
      onchange={() => data.onCommit?.()}
    ></textarea>
    <div class="opaque-note">verbatim LashVm · one statement · rendered as-is</div>
  {:else}
    <pre class="opaque-ro">{node.data.source ?? ''}</pre>
    <div class="opaque-note">raw statement · switch to Power to edit</div>
  {/if}
  <Handle type="source" position={Position.Bottom} />
</div>

<style>
  .wf-node.opaque {
    position: relative;
    background: linear-gradient(180deg, var(--node-hi), var(--node));
    border: 1px solid var(--line);
    border-left: 3px solid var(--accent);
    border-radius: 12px;
    padding: 9px 11px 11px;
    box-shadow: var(--shadow);
  }
  .wf-head {
    display: flex;
    align-items: center;
    gap: 7px;
    margin-bottom: 6px;
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
  }
  .wf-glyph {
    font-size: 10px;
  }
  .wf-sub {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--text-faint);
    flex: 1;
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
    width: 20px;
    height: 20px;
    border-radius: 6px;
    padding: 0;
  }
  .wf-del:hover {
    color: var(--rose);
    background: color-mix(in srgb, var(--rose) 16%, transparent);
  }
  .wf-title {
    width: 100%;
    background: transparent;
    border: none;
    border-bottom: 1px dashed transparent;
    color: var(--text);
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 13px;
    padding: 1px 0 3px;
    margin-bottom: 6px;
  }
  .wf-title:focus {
    outline: none;
    border-bottom-color: var(--accent);
  }
  .opaque-src {
    width: 100%;
    background: #0a0d13;
    border: 1px solid var(--line);
    border-radius: 8px;
    color: #ffd9c6;
    font-family: var(--font-mono);
    font-size: 11.5px;
    line-height: 1.5;
    padding: 9px 10px;
    resize: vertical;
    tab-size: 2;
  }
  .opaque-src:focus {
    outline: none;
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 14%, transparent);
  }
  .opaque-ro {
    margin: 0;
    width: 100%;
    background: #0a0d13;
    border: 1px dashed var(--line-strong);
    border-radius: 8px;
    color: #ffd9c6;
    font-family: var(--font-mono);
    font-size: 11.5px;
    line-height: 1.5;
    padding: 9px 10px;
    white-space: pre-wrap;
    word-break: break-word;
    max-height: 260px;
    overflow: auto;
  }
  .opaque-note {
    margin-top: 6px;
    font-family: var(--font-mono);
    font-size: 9px;
    color: var(--text-faint);
    letter-spacing: 0.03em;
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
  }
  .wf-node.is-done {
    border-left-color: #5fd08a;
  }
  .wf-node.is-failed {
    border-color: var(--rose);
  }
</style>
