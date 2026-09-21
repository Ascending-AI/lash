<script>
  import { getContext, setContext } from 'svelte';
  import StepCard from './StepCard.svelte';
  import AddStepButton from './AddStepButton.svelte';
  import { presentSteps } from '../../lib/steps.js';
  import { hasFacets } from '../../lib/facets.js';

  // The plain-language STEP-LIST surface (FIG-391), rebuilt as a Zapier-style
  // vertical CARD FLOW. A non-technical person reads the workflow top-to-bottom
  // as a stack of big cards connected by a rail, and tweaks each card's values in
  // place. It consumes the SAME draft document the canvas does; edits flow
  // through the same commit/save path (App owns dirty/history/Save-block).
  let { doc, onCommit, onDelete, onInsert } = $props();

  setContext('stepsHandlers', {
    onCommit: (...args) => onCommit?.(...args),
    onDelete: (...args) => onDelete?.(...args),
    onInsert: (...args) => onInsert?.(...args),
  });
  const ops = getContext('ops');
  const catalog = $derived(ops?.entries ?? []);

  const view = $derived(presentSteps(doc));
  const facetsOn = $derived(hasFacets(doc));
  const hasSteps = $derived(view.flat ? view.steps.length : view.processes.length || view.main.length);
</script>

<div class="steps-surface">
  <div class="intro">
    <h1>Your workflow, step by step</h1>
    <p>Read it top to bottom. Click a card to open its settings, tweak any value, then Save.</p>
    {#if !facetsOn}
      <p class="warn">This workflow was built by an older engine, so type hints are unavailable.</p>
    {/if}
  </div>

  {#if view.flat}
    <!-- A single-process workflow: its body IS the primary flow, shown flat on
         the root rail (no "Background tasks" wrapper, no process card). -->
    {#if view.steps.length}
      <section class="block">
        <div class="flow">
          {#if onInsert && view.insertTarget}
            <AddStepButton
              {catalog}
              topLevel={false}
              label="Add a step at the start"
              menuPlacement="below"
              onPick={(op) => onInsert(view.insertTarget, 0, op)}
            />
          {/if}
          {#each view.steps as entry, i (entry.id)}
            <StepCard {entry} {catalog} index={i} depth={0} primary={true} />
            {#if onInsert && view.insertTarget}
              <AddStepButton
                {catalog}
                topLevel={false}
                label="Add a step here"
                onPick={(op) => onInsert(view.insertTarget, i + 1, op)}
              />
            {/if}
          {/each}
        </div>
      </section>
    {/if}
  {:else}
    {#if view.processes.length}
      <section class="block">
        <div class="block-label">Background tasks</div>
        <div class="flow flow--loose">
          {#each view.processes as entry, i (entry.id)}
            <StepCard {entry} {catalog} index={i} depth={0} primary={false} />
          {/each}
        </div>
      </section>
    {/if}

    {#if view.main.length}
      <section class="block">
        <div class="block-label">Steps</div>
        <div class="flow">
          {#if onInsert}
            <AddStepButton
              {catalog}
              label="Add a step at the start"
              menuPlacement="below"
              onPick={(op) => onInsert({ main: true }, 0, op)}
            />
          {/if}
          {#each view.main as entry, i (entry.id)}
            <StepCard {entry} {catalog} index={i} depth={0} primary={true} />
            {#if onInsert}
              <AddStepButton
                {catalog}
                label="Add a step here"
                onPick={(op) => onInsert({ main: true }, i + 1, op)}
              />
            {/if}
          {/each}
        </div>
      </section>
    {/if}
  {/if}

  {#if !hasSteps}
    <div class="empty">
      <p>This workflow has no steps yet.</p>
      {#if onInsert}
        <div class="empty-add">
          <AddStepButton
            {catalog}
            topLevel={!view.flat}
            label="Add the first step"
            menuPlacement="below"
            onPick={(op) => onInsert(view.flat && view.insertTarget ? view.insertTarget : { main: true }, 0, op)}
          />
        </div>
      {/if}
    </div>
  {/if}
</div>

<style>
  .steps-surface {
    height: 100%;
    overflow-y: auto;
    padding: 26px 30px 100px;
    max-width: 760px;
    margin: 0 auto;
  }
  .intro {
    margin-bottom: 14px;
  }
  .intro h1 {
    font-size: 22px;
    font-weight: 700;
    margin: 0 0 4px;
    color: var(--text);
  }
  .intro p {
    margin: 0;
    font-size: 13.5px;
    color: var(--text-dim);
  }
  .intro .warn {
    margin-top: 6px;
    color: var(--amber);
    font-size: 12px;
  }
  .block {
    margin-top: 14px;
  }
  .block-label {
    font-family: var(--font-mono);
    font-size: 10px;
    letter-spacing: 0.16em;
    text-transform: uppercase;
    color: var(--text-faint);
    padding: 0 2px 8px;
    margin-bottom: 2px;
  }
  /* The card flow: a vertical stack; AddStepButton draws the rail segments that */
  .flow {
    display: flex;
    flex-direction: column;
  }
  .flow--loose {
    gap: 14px;
  }
  .empty {
    padding: 40px;
    text-align: center;
    color: var(--text-faint);
    font-size: 14px;
  }
  .empty-add {
    display: flex;
    justify-content: center;
    margin-top: 10px;
  }
</style>
