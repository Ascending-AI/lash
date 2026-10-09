<script>
  import { parseLiteral, encodeLiteral, operandType } from '../lib/fields.js';

  // A typed operand editor: [var | number | text | true·false] + a value input,
  // emitting canonical LashVm (`count`, `3`, `"done"`, `true`). Reused by the
  // value builder, both comparison operands, and each list item, so string
  // values pick up their quotes automatically and an in-scope variable can be
  // picked instead of a literal. The `var` option only appears when the parent
  // supplies `availableVars`. Compound values never reach it (the parent routes
  // those to the raw editor).
  let { value = '', onChange, compact = false, availableVars = [] } = $props();

  const vars = $derived(Array.isArray(availableVars) ? availableVars : []);

  let type = $state('string'); // 'var' | 'number' | 'string' | 'boolean'
  let text = $state(''); // literal text for scalar types
  let varName = $state(''); // selected variable for type === 'var'
  let editing = $state(false);

  $effect(() => {
    if (editing) return;
    const kind = operandType(value, vars);
    if (kind === 'var') {
      type = 'var';
      varName = (value ?? '').trim();
    } else if (kind !== 'expression') {
      type = kind;
      text = parseLiteral(value).value;
    }
  });

  function emit() {
    onChange?.(type === 'var' ? varName : encodeLiteral(type, text));
  }
  function onType(next) {
    type = next;
    if (next === 'var') {
      if (!vars.includes(varName)) varName = vars[0] ?? '';
    } else if (next === 'boolean' && text !== 'true' && text !== 'false') {
      text = 'true';
    } else if (next === 'number' && !/^-?\d*\.?\d*$/.test(text)) {
      text = '';
    }
    emit();
  }
</script>

<span class="sb" class:compact>
  <select
    class="sb-type"
    value={type}
    onpointerdown={(e) => e.stopPropagation()}
    onchange={(e) => onType(e.currentTarget.value)}
  >
    {#if vars.length}<option value="var">var</option>{/if}
    <option value="number">num</option>
    <option value="string">text</option>
    <option value="boolean">bool</option>
  </select>
  {#if type === 'var'}
    <select
      class="sb-val sb-var"
      value={varName}
      onpointerdown={(e) => e.stopPropagation()}
      onchange={(e) => {
        varName = e.currentTarget.value;
        emit();
      }}
    >
      {#if !vars.includes(varName)}<option value={varName}>{varName || '— variable —'}</option>{/if}
      {#each vars as v (v)}<option value={v}>{v}</option>{/each}
    </select>
  {:else if type === 'boolean'}
    <select
      class="sb-val"
      value={text}
      onpointerdown={(e) => e.stopPropagation()}
      onchange={(e) => {
        text = e.currentTarget.value;
        emit();
      }}
    >
      <option value="true">true</option>
      <option value="false">false</option>
    </select>
  {:else}
    <input
      class="sb-val"
      type={type === 'number' ? 'number' : 'text'}
      value={text}
      placeholder={type === 'number' ? '0' : 'text'}
      spellcheck="false"
      onpointerdown={(e) => e.stopPropagation()}
      onfocus={() => (editing = true)}
      oninput={(e) => {
        text = e.currentTarget.value;
        emit();
      }}
      onblur={() => (editing = false)}
    />
  {/if}
</span>

<style>
  .sb {
    display: inline-flex;
    align-items: stretch;
    gap: 3px;
    min-width: 0;
    flex: 1;
  }
  .sb-type,
  .sb-val {
    background: #0a0d13;
    border: 1px solid color-mix(in srgb, var(--accent, var(--cyan)) 26%, var(--line));
    border-radius: 6px;
    color: #eaf2ff;
    font-family: var(--font-mono);
    font-size: 11px;
    padding: 4px 6px;
    min-width: 0;
  }
  .sb-type {
    flex: 0 0 auto;
    color: var(--text-dim);
    cursor: pointer;
  }
  .sb-val {
    flex: 1;
    width: 100%;
  }
  .sb-var {
    color: var(--accent, var(--cyan));
    font-weight: 600;
    cursor: pointer;
  }
  .sb-type:focus,
  .sb-val:focus {
    outline: none;
    border-color: var(--accent, var(--cyan));
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--accent, var(--cyan)) 14%, transparent);
  }
  .compact .sb-val {
    flex: 1 1 50px;
  }
</style>
