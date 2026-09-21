<script>
  import { getContext } from 'svelte';

  // The canonical-source pane.
  // In Simplified it is a read-only peek at "what am I building".
  // In Power it is an editable code panel: edits are debounced and handed to `onProject`
  // (text→graph); the parent adopts the returned document and feeds back the new canonical
  // `source`, closing the loop.
  let { source = '', version, dirty, onProject } = $props();

  const mode = getContext('mode');

  let text = $state('');
  let focused = $state(false);
  let projecting = $state(false);
  let error = $state(null);
  let unsupported = $state(false);
  let timer = null;

  // Keep the editor mirrored to the canonical source whenever the graph changes
  // underneath us — but never clobber text the user is actively typing.
  $effect(() => {
    const incoming = source;
    if (!focused) text = incoming;
  });

  const editable = $derived(mode.power && !!onProject && !unsupported);

  async function project(next) {
    if (!onProject) return;
    projecting = true;
    const result = await onProject(next);
    projecting = false;
    if (result?.unsupported) {
      unsupported = true;
      error = null;
      return;
    }
    error = result?.ok ? null : (result?.error?.message ?? 'could not parse source');
  }

  function onInput(event) {
    text = event.currentTarget.value;
    clearTimeout(timer);
    const next = text;
    timer = setTimeout(() => project(next), 500);
  }
</script>

<section class="src">
  <header class="src-head">
    <div class="src-title">
      canonical source
      <span class="src-ver">v{version}</span>
      {#if editable}<span class="src-edit">editable</span>{/if}
    </div>
    {#if projecting}
      <span class="src-dirty">projecting…</span>
    {:else if dirty}
      <span class="src-dirty">unsaved edits</span>
    {:else}
      <span class="src-clean">saved · lash-rendered</span>
    {/if}
  </header>
  {#if editable}
    <textarea
      class="src-input"
      spellcheck="false"
      value={text}
      oninput={onInput}
      onfocus={() => (focused = true)}
      onblur={() => (focused = false)}
    ></textarea>
    {#if error}
      <div class="src-error">{error}</div>
    {/if}
  {:else}
    <pre class="src-body"><code>{source}</code></pre>
  {/if}
  <div class="src-foot">
    {#if editable}
      edit to reshape the graph · code ⇄ graph through the lens
    {:else if mode.power}
      graph→code output · live text editing needs a newer backend
    {:else}
      graph→code output · positions never appear here
    {/if}
  </div>
</section>

<style>
  .src {
    display: flex;
    flex-direction: column;
    background: linear-gradient(180deg, rgba(18, 23, 34, 0.7), rgba(12, 16, 24, 0.7));
    border: 1px solid var(--line);
    border-radius: 14px;
    overflow: hidden;
    min-height: 0;
  }
  .src-head {
    display: flex;
    justify-content: space-between;
    align-items: center;
    padding: 12px 15px 10px;
    border-bottom: 1px solid var(--line);
  }
  .src-title {
    display: flex;
    align-items: center;
    gap: 8px;
    font-weight: 600;
    font-size: 13px;
  }
  .src-ver {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--cyan);
    background: color-mix(in srgb, var(--cyan) 14%, transparent);
    padding: 1px 6px;
    border-radius: 5px;
  }
  .src-edit {
    font-family: var(--font-mono);
    font-size: 8.5px;
    letter-spacing: 0.1em;
    text-transform: uppercase;
    color: var(--violet);
    background: color-mix(in srgb, var(--violet) 16%, transparent);
    padding: 1px 6px;
    border-radius: 5px;
  }
  .src-dirty {
    font-family: var(--font-mono);
    font-size: 9.5px;
    color: var(--amber);
    letter-spacing: 0.06em;
  }
  .src-clean {
    font-family: var(--font-mono);
    font-size: 9.5px;
    color: var(--text-faint);
    letter-spacing: 0.04em;
  }
  .src-body {
    margin: 0;
    padding: 13px 15px;
    overflow: auto;
    max-height: 260px;
    background: #0a0d13;
  }
  .src-body code {
    font-family: var(--font-mono);
    font-size: 11.5px;
    line-height: 1.6;
    color: #cfe3ff;
    white-space: pre;
  }
  .src-input {
    margin: 0;
    border: none;
    border-left: 2px solid color-mix(in srgb, var(--violet) 45%, transparent);
    padding: 13px 15px;
    max-height: 320px;
    min-height: 120px;
    resize: vertical;
    background: #0a0d13;
    color: #eae2ff;
    font-family: var(--font-mono);
    font-size: 11.5px;
    line-height: 1.6;
    tab-size: 2;
    white-space: pre;
  }
  .src-input:focus {
    outline: none;
    border-left-color: var(--violet);
  }
  .src-error {
    padding: 8px 15px;
    background: color-mix(in srgb, var(--rose) 12%, transparent);
    border-top: 1px solid color-mix(in srgb, var(--rose) 32%, transparent);
    color: var(--rose);
    font-family: var(--font-mono);
    font-size: 10px;
    line-height: 1.45;
  }
  .src-foot {
    padding: 8px 15px;
    border-top: 1px solid var(--line);
    font-family: var(--font-mono);
    font-size: 9px;
    color: var(--text-faint);
    letter-spacing: 0.03em;
  }
</style>
