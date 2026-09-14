<script>
  import { validateFragment } from '../lib/api.js';

  // A name input that validates as a Lashlang `identifier` inline (via the same
  // /validate path ExpressionField uses for expressions), so a bad process name,
  // param/signal name, loop binding, or a brand-new assignment target fails at
  // the field with a red underline + message instead of only at Save. `variant`
  // picks the look: 'title' (borderless, large) or 'box' (bordered, compact).
  // When `options` is non-empty the input becomes a combobox (a native datalist)
  // — pick an in-scope name or type a new identifier.
  let {
    value = '',
    onInput,
    onCommit,
    placeholder = 'name',
    variant = 'box',
    ariaLabel = 'identifier',
    options = [],
  } = $props();

  const opts = $derived(Array.isArray(options) ? options : []);
  const listId = `idf-dl-${Math.random().toString(36).slice(2)}`;

  let error = $state(null);
  let timer = null;
  let seq = 0;

  async function runValidate(text) {
    const trimmed = (text ?? '').trim();
    if (!trimmed) {
      error = null;
      return;
    }
    const mine = (seq += 1);
    const res = await validateFragment('identifier', trimmed, options ?? []);
    if (mine !== seq) return;
    error = res.ok ? null : (res.error?.message ?? 'invalid identifier');
  }
  function onInputEvent(event) {
    const text = event.currentTarget.value;
    onInput?.(text);
    clearTimeout(timer);
    timer = setTimeout(() => runValidate(text), 400);
  }
  function onBlur() {
    clearTimeout(timer);
    runValidate(value);
    onCommit?.();
  }
</script>

<span class="idf idf--{variant}" class:has-error={!!error} title={error ?? ''}>
  <input
    class="idf-input"
    {value}
    {placeholder}
    aria-label={ariaLabel}
    spellcheck="false"
    list={opts.length ? listId : undefined}
    oninput={onInputEvent}
    onblur={onBlur}
  />
  {#if opts.length}
    <datalist id={listId}>
      {#each opts as o (o)}<option value={o}></option>{/each}
    </datalist>
  {/if}
</span>

<style>
  .idf {
    display: inline-flex;
    align-items: center;
    min-width: 0;
    flex: 1;
  }
  .idf-input {
    width: 100%;
    min-width: 0;
    color: var(--text);
    font-family: var(--font-mono);
  }
  .idf-input:focus {
    outline: none;
  }

  .idf--box .idf-input {
    background: #0a0d13;
    border: 1px solid color-mix(in srgb, var(--accent) 30%, var(--line));
    border-radius: 6px;
    font-size: 11px;
    padding: 4px 6px;
  }
  .idf--box .idf-input:focus {
    border-color: var(--accent);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent) 14%, transparent);
  }
  .idf--box.has-error .idf-input {
    border-color: var(--rose);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--rose) 18%, transparent);
  }

  .idf--title .idf-input {
    background: transparent;
    border: none;
    border-bottom: 1px dashed transparent;
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 14px;
    padding: 1px 0;
  }
  .idf--title .idf-input:focus {
    border-bottom-color: var(--accent);
  }
  .idf--title.has-error .idf-input {
    border-bottom: 1px dashed var(--rose);
    color: var(--rose);
  }

  /* borderless inline name — used inside a bordered signature-row pill */
  .idf--bare .idf-input {
    background: transparent;
    border: none;
    border-bottom: 1px dashed transparent;
    color: var(--accent);
    font-weight: 600;
    font-size: 10.5px;
    padding: 1px 0;
  }
  .idf--bare .idf-input:focus {
    border-bottom-color: var(--accent);
  }
  .idf--bare.has-error .idf-input {
    border-bottom-color: var(--rose);
    color: var(--rose);
  }
</style>
