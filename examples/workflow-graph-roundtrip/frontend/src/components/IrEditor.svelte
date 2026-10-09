<script>
  import { applyEdits, fetchWorkflowIr } from '../lib/api.js';
  import { slotLabel } from '../lib/irSlots.js';

  // The generic structured editor. The forms on the canvas cover the common
  // nodes; this panel reaches everything else. It shows a node as the typed
  // statement Lash holds for it and lets any expression of that statement be
  // replaced, by its slot path, with typed IR. An edit is applied to the saved
  // version as one transaction and published. No source text is involved.
  let { version, dirty, nodes = [], selected = [], onEdited } = $props();

  let open = $state(false);
  let workflow = $state(null);
  let loadError = $state(null);
  let nodeId = $state('');
  let slotIndex = $state(-1);
  let text = $state('');
  let transaction = $state('');
  let error = $state(null);
  let applying = $state(false);

  // The typed document belongs to one saved version: read it again whenever
  // the panel is open on a version it has not read.
  $effect(() => {
    if (!open || dirty || workflow?.version === version) return;
    const wanted = version;
    fetchWorkflowIr()
      .then((read) => {
        if (wanted !== version) return;
        workflow = read;
        loadError = null;
      })
      .catch((err) => {
        loadError = err?.message ?? String(err);
      });
  });

  // Follow the canvas selection until the reader picks a node here.
  $effect(() => {
    const picked = selected[0];
    if (picked && picked !== nodeId && workflow?.nodes?.[picked]) choose(picked);
  });

  const editable = $derived(
    nodes.filter((n) => n.data.kind !== 'process' && workflow?.nodes?.[n.id]),
  );
  const current = $derived(workflow?.nodes?.[nodeId] ?? null);
  const slots = $derived(current?.slots ?? []);

  function choose(id) {
    nodeId = id;
    pick(-1);
  }

  // -1 is the whole statement; any other index is one of its slots.
  function pick(index) {
    slotIndex = index;
    error = null;
    const node = workflow?.nodes?.[nodeId];
    const value = index === -1 ? node?.statement : node?.slots?.[index]?.expression;
    text = value === undefined ? '' : JSON.stringify(value, null, 2);
  }

  async function send(edits) {
    applying = true;
    error = null;
    const result = await applyEdits(version, edits);
    applying = false;
    if (!result.ok) {
      error = result.error ?? { code: `http_${result.status}`, message: 'the edit was refused' };
      return;
    }
    workflow = null;
    nodeId = result.idMap?.[nodeId] ?? '';
    onEdited?.(result);
  }

  function parsed(source) {
    try {
      return JSON.parse(source);
    } catch (err) {
      error = { code: 'invalid_json', message: err?.message ?? String(err) };
      return undefined;
    }
  }

  function replace() {
    const expression = parsed(text);
    if (expression === undefined) return;
    if (slotIndex === -1) {
      send([{ op: 'replaceNode', node: nodeId, statement: expression }]);
    } else {
      send([
        { op: 'replaceExpression', node: nodeId, slot: slots[slotIndex].path, expression },
      ]);
    }
  }

  function applyTransaction() {
    const edits = parsed(transaction);
    if (edits === undefined) return;
    if (!Array.isArray(edits)) {
      error = { code: 'invalid_transaction', message: 'a transaction is a JSON list of edits' };
      return;
    }
    send(edits);
  }
</script>

<section class="ir">
  <button class="ir-head" onclick={() => (open = !open)} aria-expanded={open}>
    <span class="ir-title">Structured editor</span>
    <span class="ir-sub">typed IR · every construct</span>
    <span class="ir-caret">{open ? '▾' : '▸'}</span>
  </button>
  {#if open}
    <div class="ir-body">
      {#if dirty}
        <p class="ir-note">Save the form edits first: a typed edit applies to the saved version.</p>
      {:else if loadError}
        <p class="ir-err">{loadError}</p>
      {:else if !workflow}
        <p class="ir-note">reading the typed document…</p>
      {:else}
        <label class="ir-label" for="ir-node">node</label>
        <select
          id="ir-node"
          class="ir-select"
          value={nodeId}
          onchange={(e) => choose(e.currentTarget.value)}
        >
          <option value="" disabled>select a node</option>
          {#each editable as n (n.id)}
            <option value={n.id}>{n.data.subkind ?? n.data.kind} · {n.data.title}</option>
          {/each}
        </select>

        {#if current}
          <label class="ir-label" for="ir-slot">expression</label>
          <select
            id="ir-slot"
            class="ir-select"
            value={slotIndex}
            onchange={(e) => pick(Number(e.currentTarget.value))}
          >
            <option value={-1}>whole statement</option>
            {#each slots as slot, i (i)}
              <option value={i}>{slotLabel(slot)}</option>
            {/each}
          </select>
          <textarea
            class="ir-text"
            spellcheck="false"
            rows="9"
            aria-label="Typed IR"
            value={text}
            oninput={(e) => (text = e.currentTarget.value)}
          ></textarea>
          <button class="ir-apply" disabled={applying} onclick={replace}>
            {slotIndex === -1 ? 'Replace statement' : 'Replace expression'}
          </button>
        {/if}

        <label class="ir-label" for="ir-tx">edit transaction</label>
        <textarea
          id="ir-tx"
          class="ir-text"
          spellcheck="false"
          rows="5"
          placeholder={'[{ "op": "setCatch", "node": "…", "binding": "error" }]'}
          value={transaction}
          oninput={(e) => (transaction = e.currentTarget.value)}
        ></textarea>
        <button class="ir-apply" disabled={applying || !transaction.trim()} onclick={applyTransaction}>
          Apply transaction
        </button>
      {/if}
      {#if error}
        <p class="ir-err" role="alert">
          <strong>{error.code}</strong>
          {error.message}
        </p>
      {/if}
    </div>
  {/if}
</section>

<style>
  .ir {
    display: flex;
    flex-direction: column;
    background: linear-gradient(180deg, rgba(18, 23, 34, 0.7), rgba(12, 16, 24, 0.7));
    border: 1px solid var(--line);
    border-radius: 14px;
    overflow: hidden;
  }
  .ir-head {
    display: flex;
    align-items: baseline;
    gap: 8px;
    padding: 12px 15px 10px;
    background: transparent;
    border: none;
    color: var(--text);
    text-align: left;
    cursor: pointer;
  }
  .ir-title {
    font-weight: 600;
    font-size: 13px;
  }
  .ir-sub {
    flex: 1;
    font-family: var(--font-mono);
    font-size: 9.5px;
    color: var(--text-faint);
    letter-spacing: 0.04em;
  }
  .ir-caret {
    color: var(--text-faint);
  }
  .ir-body {
    display: flex;
    flex-direction: column;
    gap: 7px;
    padding: 12px 15px 14px;
    border-top: 1px solid var(--line);
  }
  .ir-label {
    font-family: var(--font-mono);
    font-size: 9.5px;
    letter-spacing: 0.08em;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  .ir-select,
  .ir-text {
    width: 100%;
    background: #0a0d13;
    border: 1px solid var(--line);
    border-radius: 8px;
    color: var(--text);
    font-family: var(--font-mono);
    font-size: 11.5px;
    padding: 7px 9px;
  }
  .ir-text {
    line-height: 1.5;
    resize: vertical;
    tab-size: 2;
  }
  .ir-select:focus,
  .ir-text:focus {
    outline: none;
    border-color: var(--cyan);
  }
  .ir-apply {
    align-self: flex-start;
    background: color-mix(in srgb, var(--cyan) 16%, transparent);
    border: 1px solid color-mix(in srgb, var(--cyan) 40%, transparent);
    border-radius: 8px;
    color: var(--cyan);
    font-size: 12px;
    padding: 5px 11px;
    cursor: pointer;
  }
  .ir-apply:disabled {
    opacity: 0.5;
    cursor: default;
  }
  .ir-note {
    margin: 0;
    font-size: 12px;
    color: var(--text-faint);
  }
  .ir-err {
    margin: 0;
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--rose);
    word-break: break-word;
  }
</style>
