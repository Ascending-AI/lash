<script>
  import { operationMeta, groupOperations } from '../../lib/operations.js';

  // The "+" insert affordance that lives ON THE RAIL between two cards (item 4).
  // It sits quietly as a hairline until hovered/opened, then reveals a small
  // circular button; tapping it opens the operation palette (the SAME
  // host-owned catalog + grouping the canvas palette uses) and hands the chosen
  // operation back to the parent, which wires it through the existing add-node
  // flow at this exact position.
  let {
    catalog = [],
    topLevel = true,
    onPick,
    label = 'Add a step here',
    menuPlacement = 'center',
  } = $props();

  let open = $state(false);
  const groups = $derived(groupOperations(catalog, { includePower: false, topLevel }));

  function choose(op) {
    open = false;
    onPick?.(op);
  }
  function onBlur(event) {
    if (!event.currentTarget.contains(event.relatedTarget)) open = false;
  }
</script>

<div class="rail-add" class:is-open={open} onfocusout={onBlur}>
  <span class="rail-line rail-line--top" aria-hidden="true"></span>
  <button
    class="add-dot"
    type="button"
    title={label}
    aria-label={label}
    aria-expanded={open}
    onclick={() => (open = !open)}
  >
    <span class="add-plus">+</span>
  </button>
  <span class="rail-line rail-line--bot" aria-hidden="true"></span>

  {#if open}
    <div class="add-menu" class:is-below={menuPlacement === 'below'} role="menu">
      {#if groups.length}
        {#each groups as grp (grp.id)}
          <div class="add-group">{grp.label}</div>
          {#each grp.items as op (op.id)}
            {@const m = operationMeta(op)}
            <button
              class="add-item"
              type="button"
              role="menuitem"
              style="--c:{m.accent}"
              onclick={() => choose(op)}
            >
              <span class="add-glyph">{m.glyph}</span>{op.label}
            </button>
          {/each}
        {/each}
      {:else}
        <div class="add-empty">No steps available to add.</div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .rail-add {
    position: relative;
    display: flex;
    flex-direction: column;
    align-items: center;
    /* The connector segment between two cards. */
    height: 34px;
  }
  .rail-line {
    width: 2px;
    flex: 1;
    background: var(--line-strong);
  }
  .add-dot {
    display: inline-flex;
    align-items: center;
    justify-content: center;
    width: 22px;
    height: 22px;
    flex-shrink: 0;
    border-radius: 50%;
    border: 1.5px solid var(--line-strong);
    background: var(--ink-2);
    color: var(--text-faint);
    cursor: pointer;
    opacity: 0;
    transform: scale(0.82);
    transition:
      opacity 0.14s ease,
      transform 0.14s ease,
      color 0.14s ease,
      border-color 0.14s ease,
      background 0.14s ease;
  }
  .add-plus {
    font-size: 16px;
    line-height: 1;
    margin-top: -1px;
  }
  /* Reveal the dot on hover/focus of the connector, or while its menu is open. */
  .rail-add:hover .add-dot,
  .rail-add:focus-within .add-dot,
  .rail-add.is-open .add-dot {
    opacity: 1;
    transform: scale(1);
  }
  .add-dot:hover,
  .rail-add.is-open .add-dot {
    color: var(--cyan);
    border-color: var(--cyan);
    background: color-mix(in srgb, var(--cyan) 14%, var(--ink-2));
    box-shadow: 0 0 0 4px color-mix(in srgb, var(--cyan) 10%, transparent);
  }

  .add-menu {
    position: absolute;
    top: 50%;
    left: calc(50% + 22px);
    transform: translateY(-50%);
    z-index: 20;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 6px;
    min-width: 220px;
    max-height: 320px;
    overflow-y: auto;
    background: var(--ink-2);
    border: 1px solid var(--line-strong);
    border-radius: 12px;
    box-shadow: 0 18px 42px -16px rgba(0, 0, 0, 0.76);
  }
  /* The first rail control sits immediately below the sticky top bar. menu downward so the leading actions remain visible and clickable instead of occupying the top bar's pointer layer. */
  .add-menu.is-below {
    top: calc(50% + 14px);
    transform: none;
  }
  .add-group {
    font-family: var(--font-mono);
    font-size: 8px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--text-faint);
    padding: 6px 8px 2px;
  }
  .add-item {
    display: inline-flex;
    align-items: center;
    gap: 8px;
    font-family: var(--font-ui);
    font-size: 13px;
    color: var(--text-dim);
    background: transparent;
    border: 1px solid transparent;
    border-radius: 8px;
    padding: 7px 9px;
    cursor: pointer;
    text-align: left;
    transition:
      color 0.12s ease,
      background 0.12s ease,
      border-color 0.12s ease;
  }
  .add-item:hover {
    color: var(--text);
    background: color-mix(in srgb, var(--c) 14%, transparent);
    border-color: color-mix(in srgb, var(--c) 45%, transparent);
  }
  .add-glyph {
    color: var(--c);
    font-size: 13px;
    width: 16px;
    text-align: center;
    flex-shrink: 0;
  }
  .add-empty {
    font-size: 12px;
    color: var(--text-faint);
    padding: 10px 8px;
    text-align: center;
  }
</style>
