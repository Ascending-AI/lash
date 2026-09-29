<script>
  import { editableKind, editableText } from '../../lib/editableValue.js';
  import ExpressionField from '../ExpressionField.svelte';
  import ValueToken from './ValueToken.svelte';
  import { describeValue } from '../../lib/steps.js';
  import { expectedArgFieldType, enumMembers, enumMemberFromText } from '../../lib/facets.js';

  // One labeled field of an action card (a call node's typed `data.fields`
  // argument). Reads as a calm chip; taps open the right editor for the value's
  // type — an enum dropdown, a plain text/number/yes-no control, or (for an
  // expression-valued arg) the shared facet-driven ExpressionField whose
  // variable picker offers only type-compatible tokens.
  let { node, fieldKey, onCommit } = $props();

  const value = $derived(node.data.fields?.[fieldKey]);
  const expected = $derived(expectedArgFieldType(node, fieldKey));
  const enumOpts = $derived(enumMembers(expected));
  const availableVars = $derived(node.data.availableVars ?? []);

  let editing = $state(false);
  function open() {
    if (kind === 'object' || kind === 'list' || kind === 'null') return;
    editing = true;
  }
  function close() {
    editing = false;
    onCommit?.();
  }

  const kind = $derived(editableKind(value));

  // A read descriptor for the calm chip. Expression args go through
  // describeValue (token-capable); plain literals paint directly.
  const desc = $derived.by(() => {
    if (kind === 'expr') return describeValue(value.value, availableVars);
    if (kind === 'number') return { kind: 'static', literalType: 'number', display: editableText(value) };
    if (kind === 'bool')
      return { kind: 'static', literalType: 'boolean', display: value.value ? 'Yes' : 'No' };
    return { kind: 'static', literalType: 'string', display: editableText(value) };
  });

  function autofocus(el) {
    const c = el.querySelector('input, textarea, select');
    if (c) queueMicrotask(() => c.focus());
  }
</script>

<span class="arg">
  <span class="arg-key">{fieldKey}</span>
  {#if !editing}
    <button class="arg-read" type="button" title="Tap to change" onclick={open}>
      <ValueToken {desc} placeholder="not set" />
      <span class="arg-pen" aria-hidden="true">✎</span>
    </button>
  {:else if enumOpts && kind === 'string'}
    <span class="arg-edit" use:autofocus>
      <select
        class="arg-select"
        value={enumMemberFromText(value.value, enumOpts) ?? '__custom__'}
        onchange={(e) => {
          node.data.fields[fieldKey] = { kind: 'string', value: e.currentTarget.value };
        }}
        onblur={close}
      >
        {#if enumMemberFromText(value.value, enumOpts) === null}
          <option value="__custom__" disabled>{value.value || '— choose —'}</option>
        {/if}
        {#each enumOpts as m (m)}<option value={m}>{m}</option>{/each}
      </select>
    </span>
  {:else if kind === 'number'}
    <span class="arg-edit" use:autofocus>
      <input
        class="arg-input"
        type="number"
        value={value.value}
        oninput={(e) => {
          const n = Number(e.currentTarget.value);
          if (!Number.isNaN(n)) node.data.fields[fieldKey] = { kind: 'number', value: n };
        }}
        onblur={close}
      />
    </span>
  {:else if kind === 'bool'}
    <span class="arg-edit" use:autofocus>
      <select
        class="arg-select"
        value={value.value ? 'true' : 'false'}
        onchange={(e) => {
          node.data.fields[fieldKey] = { kind: 'bool', value: e.currentTarget.value === 'true' };
        }}
        onblur={close}
      >
        <option value="true">Yes</option>
        <option value="false">No</option>
      </select>
    </span>
  {:else if kind === 'expr'}
    <span class="arg-edit" use:autofocus>
      <ExpressionField
        value={value.value}
        kind="expression"
        builder="value"
        {availableVars}
        expectedType={expected}
        placeholder="value…"
        onInput={(t) => (node.data.fields[fieldKey] = { kind: 'expr', value: t })}
        onCommit={close}
      />
    </span>
  {:else}
    <span class="arg-edit" use:autofocus>
      <input
        class="arg-input"
        type="text"
        value={value.value}
        spellcheck="false"
        oninput={(e) => (node.data.fields[fieldKey] = { kind: 'string', value: e.currentTarget.value })}
        onblur={close}
      />
    </span>
  {/if}
</span>

<style>
  .arg {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    min-width: 0;
  }
  .arg-key {
    font-family: var(--font-mono);
    font-size: 10px;
    letter-spacing: 0.04em;
    text-transform: uppercase;
    color: var(--text-faint);
    min-width: 54px;
    text-align: right;
    flex-shrink: 0;
  }
  .arg-read {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    background: transparent;
    border: 1px solid transparent;
    border-radius: 8px;
    padding: 2px 5px;
    cursor: pointer;
    transition:
      background 0.12s ease,
      border-color 0.12s ease;
  }
  .arg-read:hover {
    background: color-mix(in srgb, var(--cyan) 8%, transparent);
    border-color: color-mix(in srgb, var(--cyan) 26%, var(--line));
  }
  .arg-pen {
    font-size: 10px;
    color: var(--text-faint);
    opacity: 0;
    transition: opacity 0.12s ease;
  }
  .arg-read:hover .arg-pen {
    opacity: 1;
  }
  .arg-edit {
    display: inline-flex;
    flex: 1;
    min-width: 160px;
    max-width: 360px;
  }
  .arg-input,
  .arg-select {
    width: 100%;
    background: #0a0d13;
    border: 1px solid var(--line-strong);
    border-radius: 7px;
    color: var(--text);
    font-family: var(--font-ui);
    font-size: 12.5px;
    padding: 5px 8px;
  }
  .arg-input:focus,
  .arg-select:focus {
    outline: none;
    border-color: var(--cyan);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--cyan) 14%, transparent);
  }
</style>
