<script>
  import { setContext } from 'svelte';
  import { SvelteFlow, Background, Controls, MiniMap, BackgroundVariant } from '@xyflow/svelte';
  import WorkflowNode from './components/nodes/WorkflowNode.svelte';
  import ContainerNode from './components/nodes/ContainerNode.svelte';
  import OpaqueNode from './components/nodes/OpaqueNode.svelte';
  import DisplayPanel from './components/DisplayPanel.svelte';
  import SourceView from './components/SourceView.svelte';
  import StepsView from './components/steps/StepsView.svelte';
  import {
    fetchWorkflow,
    fetchWorkflows,
    selectWorkflow,
    saveWorkflow,
    fetchOperations,
    projectSource,
  } from './lib/api.js';
  import {
    buildFlow,
    deleteNodeFromDoc,
    addNodeToDoc,
    reorderNodeInDoc,
    moveNodeToGroup,
    moveTargetsFor,
    scopeOf,
  } from './lib/graph.js';
  import {
    loadPositions,
    savePosition,
    clearPosition,
    clearPositions,
    migratePositions,
  } from './lib/positions.js';
  import { RunController } from './lib/runStore.svelte.js';
  import { ModeController } from './lib/mode.svelte.js';
  import { ViewController } from './lib/view.svelte.js';
  import { History } from './lib/history.svelte.js';
  import { groupOperations, operationMeta } from './lib/operations.js';
  import { NODE_KINDS } from './lib/nodeKinds.js';
  import { blockingDiagnostics, clearFacetDiagnostics } from './lib/facets.js';

  const run = new RunController();
  setContext('run', run);

  const mode = new ModeController();
  setContext('mode', mode);

  const view = new ViewController();
  setContext('view', view);

  // Operation catalog store — host-owned data from GET /operations, the single
  // source of truth for the palette. `entries` is null until it resolves;
  // `error` is set when the endpoint is unreachable so the palette can show a
  // non-blocking empty state instead of a duplicated hard-coded catalog.
  const ops = $state({ entries: null, error: false });
  setContext('ops', ops);

  const history = new History();

  const nodeTypes = { workflow: WorkflowNode, container: ContainerNode, opaque: OpaqueNode };

  let draftDoc = $state(null);
  let canonicalSource = $state('');
  let savedVersion = $state(0);
  // The saved version's admitted definition: the artifact a run executes and
  // the only one whose events the overlay shows.
  let savedDefinition = $state(null);
  let flowNodes = $state([]);
  let flowEdges = $state([]);
  let flowKey = $state(0);

  let positions = loadPositions();
  let dirty = $state(false);
  let loading = $state(true);
  let saving = $state(false);
  let loadError = $state(null);
  let saveError = $state(null);
  let saveOk = $state(null);

  let catalog = $state([]);
  let selectedId = $state('onboarding');
  let switching = $state(false);
  // Probed once at load so the Power source pane offers live editing only when it will
  // actually work.
  let projectSupported = $state(false);

  const legend = Object.entries(NODE_KINDS);
  const mainGroups = $derived(groupOperations(ops.entries, { includePower: mode.power }));

  // Definite type-error diagnostics on the current draft (host-derived facets).
  // These block Save (item 5). They are cleared on the next edit — a client edit
  // makes the last derivation stale, and a stale error is worse than none — so
  // the block only bites on a KNOWN-broken graph the user has not yet touched.
  const blockingDiags = $derived(draftDoc ? blockingDiagnostics(draftDoc) : []);

  // Handlers threaded into every flow node (see buildFlow).
  const handlers = {
    onDelete,
    onAddNode,
    onReorder,
    onCommit,
    onRebuild,
    onMoveTo,
    getMoveTargets: (id) => moveTargetsFor(draftDoc, id),
  };

  // Record a committed edit (blur of a field): mark the draft dirty and push a
  // history snapshot. Field values are already written live into the draft.
  function onCommit() {
    dirty = true;
    saveOk = null;
    clearFacetDiagnostics(draftDoc);
    history.commit(draftDoc);
  }

  // A structural change already applied to the draft (e.g. adding/removing a
  // comprehension clause) that also needs a relayout: commit + rebuild.
  function onRebuild() {
    dirty = true;
    saveOk = null;
    clearFacetDiagnostics(draftDoc);
    history.commit(draftDoc);
    rebuild();
  }

  function onDelete(id) {
    deleteNodeFromDoc(draftDoc, id);
    dirty = true;
    saveOk = null;
    clearFacetDiagnostics(draftDoc);
    history.commit(draftDoc);
    rebuild();
  }

  function onAddNode(ownerId, slot, operation) {
    addNodeToDoc(draftDoc, { ownerId, slot }, operation, ops.entries ?? []);
    dirty = true;
    saveOk = null;
    clearFacetDiagnostics(draftDoc);
    history.commit(draftDoc);
    rebuild();
  }

  // Reuses the existing add flow (append into the target group, honouring the
  // trailing-terminal barrier), then reorders the new node to `index` so it lands exactly
  // where the "+" sat between two cards.
  function onInsert(target, index, operation) {
    const id = addNodeToDoc(draftDoc, target, operation, ops.entries ?? []);
    if (id && Number.isFinite(index)) reorderNodeInDoc(draftDoc, id, index);
    dirty = true;
    saveOk = null;
    clearFacetDiagnostics(draftDoc);
    history.commit(draftDoc);
    rebuild(id ? new Set([id]) : null);
  }

  let mainMenuOpen = $state(false);
  function onAddMain(operation) {
    mainMenuOpen = false;
    addNodeToDoc(draftDoc, { main: true }, operation, ops.entries ?? []);
    dirty = true;
    saveOk = null;
    clearFacetDiagnostics(draftDoc);
    history.commit(draftDoc);
    rebuild();
  }

  function onReorder(id, direction) {
    if (reorderNodeInDoc(draftDoc, id, direction)) {
      dirty = true;
      saveOk = null;
      clearFacetDiagnostics(draftDoc);
      history.commit(draftDoc);
      rebuild(new Set([id]));
    }
  }

  // Snap it out of any dragged position so it lays out cleanly.
  function onMoveTo(id, dest) {
    if (moveNodeToGroup(draftDoc, id, dest)) {
      clearPosition(id);
      positions = loadPositions();
      dirty = true;
      saveOk = null;
      clearFacetDiagnostics(draftDoc);
      history.commit(draftDoc);
      rebuild(new Set([id]));
    }
  }

  function rebuild(keepSelection = null) {
    const { flowNodes: fn, flowEdges: fe } = buildFlow(draftDoc, positions, handlers);
    if (keepSelection && keepSelection.size) {
      for (const n of fn) if (keepSelection.has(n.id)) n.selected = true;
    }
    flowNodes = fn;
    flowEdges = fe;
  }

  async function loadInitial() {
    loading = true;
    loadError = null;
    try {
      const [doc, list, operations] = await Promise.all([
        fetchWorkflow(),
        fetchWorkflows().catch(() => []),
        fetchOperations().catch(() => null),
      ]);
      catalog = list;
      ops.entries = operations;
      ops.error = operations == null;
      adoptDocument(doc);
      // Probe /project support without adopting the result (projecting the
      // canonical source yields an equivalent document we discard).
      projectSource(doc.source)
        .then((probe) => {
          projectSupported = !probe.unsupported;
        })
        .catch(() => {
          projectSupported = false;
        });
    } catch (err) {
      loadError = err?.message ?? String(err);
    } finally {
      loading = false;
    }
  }

  async function onSelect(id) {
    if (switching || id === selectedId) return;
    switching = true;
    saveError = null;
    saveOk = null;
    run.stop();
    run.reset();
    try {
      const doc = await selectWorkflow(id);
      selectedId = id;
      adoptDocument(doc);
    } catch (err) {
      loadError = err?.message ?? String(err);
    } finally {
      switching = false;
    }
  }

  // A stable signature of the graph's SHAPE (kinds + nesting + slot structure), independent of
  // the node ids that every Save remints.
  function shapeSignature(doc) {
    if (!doc) return '';
    const byId = new Map((doc.nodes ?? []).map((n) => [n.id, n]));
    const sig = (id) => {
      const n = byId.get(id);
      if (!n) return '';
      const groups = (n.data?.children ?? [])
        .map((g) => `${g.slot}[${g.nodeIds.map(sig).join(',')}]`)
        .join('');
      return `${n.data?.kind}:${n.data?.subkind ?? ''}(${groups})`;
    };
    const roots = [
      ...(doc.roots?.processes ?? []).map(sig),
      '|',
      ...(doc.roots?.main ?? []).map(sig),
    ];
    return roots.join(';');
  }

  // Adopt a canonical (saved / selected) document as a fresh, clean draft.
  // `refit` re-keys the SvelteFlow instance so its viewport re-fits — wanted on a
  // workflow switch, but not on a Save that left the graph shape unchanged (the
  // migrated positions keep every node in place, so a re-fit would just jump).
  function adoptDocument(doc, keepSelection = null, { refit = true } = {}) {
    draftDoc = structuredClone($state.snapshot(doc));
    canonicalSource = doc.source;
    savedVersion = doc.version;
    savedDefinition = doc.definition ?? null;
    dirty = false;
    history.reset(draftDoc);
    if (refit) flowKey += 1;
    rebuild(keepSelection);
  }

  // Adopt a document projected from edited source (Power source pane). Unlike a
  // saved document this is an unsaved draft: keep it dirty and start a new
  // history baseline entry from it.
  function adoptProjected(doc) {
    draftDoc = structuredClone($state.snapshot(doc));
    canonicalSource = doc.source;
    dirty = true;
    saveOk = null;
    history.commit(draftDoc);
    flowKey += 1;
    rebuild();
  }

  async function handleProject(text) {
    const result = await projectSource(text);
    if (result.ok) adoptProjected(result.document);
    return result;
  }

  async function onSave() {
    if (!draftDoc) return;
    // Only real (host-derived) diagnostics block; unknowns never do.
    // The draft is kept so the user can fix the flagged node and try again.
    if (blockingDiags.length) {
      saveOk = null;
      saveError = null;
      return;
    }
    saving = true;
    saveError = null;
    saveOk = null;
    const selectedIds = flowNodes.filter((n) => n.selected).map((n) => n.id);
    const shapeBefore = shapeSignature(draftDoc);
    const payload = JSON.parse(JSON.stringify(draftDoc));
    const result = await saveWorkflow(payload);
    saving = false;
    if (result.ok) {
      let keepSelection = null;
      if (result.idMap) {
        migratePositions(result.idMap);
        positions = loadPositions();
        keepSelection = new Set(selectedIds.map((oldId) => result.idMap[oldId] ?? oldId));
      }
      // Only re-fit the viewport when the Save actually reshaped the graph;
      // a same-shape save keeps every node in place, so a re-fit would just jump.
      const refit = shapeSignature(result.document) !== shapeBefore;
      adoptDocument(result.document, keepSelection, { refit });
      saveOk = `saved as v${result.document.version}`;
      run.reset();
    } else {
      saveError = result.error ?? {
        code: `http_${result.status}`,
        message: 'save failed',
        details: {},
      };
    }
  }

  // --- Undo / redo -----------------------------------------------------------
  function applySnapshot(doc) {
    if (!doc) return;
    draftDoc = doc;
    dirty = history.index > 0;
    saveOk = null;
    rebuild();
  }
  function doUndo() {
    applySnapshot(history.undo());
  }
  function doRedo() {
    applySnapshot(history.redo());
  }
  function onKeydown(e) {
    if (!(e.metaKey || e.ctrlKey)) return;
    if (e.key.toLowerCase() !== 'z') return;
    const t = e.target;
    // Let native text undo win inside editable fields.
    if (t && (t.tagName === 'INPUT' || t.tagName === 'TEXTAREA' || t.isContentEditable)) return;
    e.preventDefault();
    if (e.shiftKey) doRedo();
    else doUndo();
  }

  function onPlay() {
    saveOk = null;
    run.start(savedDefinition);
  }
  function onResetLayout() {
    clearPositions();
    positions = {};
    rebuild();
  }
  function onReload() {
    run.stop();
    loadInitial();
  }

  // A drag that changes a node's order within its own scope becomes a reorder
  // (it snaps into the laid-out slot); otherwise it is a free arrangement that
  // persists as a stored position.
  function tryReorderByDrag(n) {
    const scope = scopeOf(draftDoc, n.id);
    if (!scope || scope.nodeIds.length < 2) return false;
    const tops = new Map(flowNodes.map((f) => [f.id, f.position?.y ?? 0]));
    const droppedTop = n.position.y;
    let index = 0;
    for (const sid of scope.nodeIds) {
      if (sid === n.id) continue;
      if ((tops.get(sid) ?? 0) < droppedTop) index += 1;
    }
    if (!reorderNodeInDoc(draftDoc, n.id, index)) return false;
    clearPosition(n.id);
    return true;
  }

  function handleDragStop({ targetNode, nodes }) {
    const moved = nodes && nodes.length ? nodes : targetNode ? [targetNode] : [];
    let reordered = false;
    for (const n of moved) {
      if (!n?.id || !n.position) continue;
      let didReorder = false;
      try {
        didReorder = tryReorderByDrag(n);
      } catch {
        didReorder = false;
      }
      if (didReorder) reordered = true;
      else savePosition(n.id, n.position);
    }
    positions = loadPositions();
    if (reordered) {
      dirty = true;
      saveOk = null;
      clearFacetDiagnostics(draftDoc);
      history.commit(draftDoc);
      rebuild();
    }
  }

  loadInitial();
</script>

<svelte:window onkeydown={onKeydown} />

<div class="studio">
  <header class="topbar">
    <div class="brand">
      <span class="mark">◧</span>
      <div class="brand-text">
        <div class="brand-title">Workflow Graph Studio</div>
        <div class="brand-sub">lashlang · code ⇄ graph ⇄ live run</div>
      </div>
    </div>

    <div class="topbar-right">
      <div class="mode-switch" role="group" aria-label="View">
        <button
          class="mode-btn"
          class:is-active={view.steps}
          onclick={() => view.set('steps')}
          title="Read the workflow as a plain-language checklist"
        >
          Steps
        </button>
        <button
          class="mode-btn"
          class:is-active={view.canvas}
          onclick={() => view.set('canvas')}
          title="The node/graph canvas editor"
        >
          Canvas
        </button>
      </div>
      {#if view.canvas}
        <div class="mode-switch" role="group" aria-label="Editor mode">
          <button
            class="mode-btn"
            class:is-active={mode.simplified}
            onclick={() => mode.set('simplified')}
            title="Guided editing for building workflows visually"
          >
            Simplified
          </button>
          <button
            class="mode-btn"
            class:is-active={mode.power}
            onclick={() => mode.set('power')}
            title="Raw Lashlang inputs, any node kind, and a live editable source pane"
          >
            Power
          </button>
        </div>
      {/if}
      <nav class="legend" class:is-hidden={view.steps}>
        {#each legend as [kind, meta] (kind)}
          <span class="legend-item" style="--c:{meta.accent}">
            <span class="legend-dot"></span>{meta.label}
          </span>
        {/each}
        <span class="legend-sep"></span>
        <span class="legend-item legend-edge"><span class="legend-line seq"></span>sequence</span>
        <span class="legend-item legend-edge"><span class="legend-line data"></span>data</span>
      </nav>
    </div>
  </header>

  <main class="body">
    <section class="canvas">
      {#if loading}
        <div class="overlay-msg">projecting graph…</div>
      {:else if loadError}
        {@const netFail = /fetch|network|failed to fetch|load failed/i.test(loadError)}
        <div class="overlay-msg error">
          {netFail ? 'backend unreachable' : "couldn't load the editor"}<br />
          <span class="mono">{loadError}</span>
          <button class="btn" onclick={onReload}>retry</button>
        </div>
      {:else if view.steps}
        {#if draftDoc}
          <StepsView doc={draftDoc} {onCommit} {onDelete} {onInsert} />
        {/if}
      {:else}
        {#key flowKey}
          <SvelteFlow
            bind:nodes={flowNodes}
            bind:edges={flowEdges}
            {nodeTypes}
            fitView
            fitViewOptions={{ padding: 0.18 }}
            minZoom={0.2}
            maxZoom={2}
            proOptions={{ hideAttribution: false }}
            onnodedragstop={handleDragStop}
          >
            <Background variant={BackgroundVariant.Dots} gap={26} size={1.4} />
            <Controls showLock={false} />
            <MiniMap
              pannable
              zoomable
              nodeColor={(n) => NODE_KINDS[n.data?.node?.data?.kind]?.accent ?? '#5c6a80'}
              maskColor="rgba(8,11,17,0.72)"
            />
          </SvelteFlow>
        {/key}
        <div class="palette">
          <button
            class="palette-btn"
            class:is-open={mainMenuOpen}
            onclick={() => (mainMenuOpen = !mainMenuOpen)}
            title="Add a node to the top-level (main) scope"
          >
            + Add node <span class="palette-scope">· main</span>
          </button>
          {#if mainMenuOpen}
            <div class="palette-menu">
              {#if mainGroups.length}
                {#each mainGroups as grp (grp.id)}
                  <div class="palette-group">{grp.label}</div>
                  {#each grp.items as op (op.id)}
                    {@const m = operationMeta(op)}
                    <button
                      class="palette-item"
                      style="--c:{m.accent}"
                      onclick={() => onAddMain(op)}
                    >
                      <span class="palette-glyph">{m.glyph}</span>{op.label}
                    </button>
                  {/each}
                {/each}
              {:else}
                <div class="palette-empty">
                  {ops.error
                    ? 'operation catalog unavailable — reload to retry'
                    : 'loading operations…'}
                </div>
              {/if}
            </div>
          {/if}
        </div>
        <div class="canvas-hint">
          {mode.simplified
            ? 'drag to reorder within a scope · use the ⤴ menu to move between scopes'
            : 'power mode · raw Lashlang everywhere · edit the source pane to reshape the graph'}
        </div>
      {/if}
    </section>

    <aside class="side">
      {#if catalog.length}
        <div class="wf-select">
          <label class="wf-select-label" for="wf-picker">workflow</label>
          <div class="wf-select-wrap">
            <select
              id="wf-picker"
              class="wf-select-input"
              value={selectedId}
              disabled={switching || loading}
              onchange={(e) => onSelect(e.currentTarget.value)}
              title={catalog.find((w) => w.id === selectedId)?.description ?? ''}
            >
              {#each catalog as w (w.id)}
                <option value={w.id} title={w.description}>{w.name}</option>
              {/each}
            </select>
            <span class="wf-select-caret">{switching ? '…' : '▾'}</span>
          </div>
          <div class="wf-select-desc">
            {catalog.find((w) => w.id === selectedId)?.description ?? ''}
          </div>
        </div>
      {/if}

      <div class="controls">
        <button
          class="btn btn-play"
          onclick={onPlay}
          disabled={loading || !!loadError}
          title="Create a new run of the saved workflow"
        >
          <span class="btn-glyph">{run.running ? '❚❚' : '▶'}</span>
          {run.running ? 'running…' : 'Play'}
        </button>
        <button
          class="btn btn-save"
          class:is-blocked={blockingDiags.length > 0}
          onclick={onSave}
          disabled={saving || loading || !!loadError || blockingDiags.length > 0}
          title={blockingDiags.length
            ? `Save blocked — ${blockingDiags.length} type error${blockingDiags.length > 1 ? 's' : ''} to fix`
            : 'Send the edited graph → graph→code → new version'}
        >
          {saving ? 'saving…' : blockingDiags.length ? 'Save · blocked' : 'Save'}
        </button>
        <div class="ctrl-minor">
          <button
            class="btn btn-ghost"
            onclick={doUndo}
            disabled={!history.canUndo}
            title="Undo (Ctrl/Cmd-Z)">undo</button
          >
          <button
            class="btn btn-ghost"
            onclick={doRedo}
            disabled={!history.canRedo}
            title="Redo (Ctrl/Cmd-Shift-Z)">redo</button
          >
          <button class="btn btn-ghost" onclick={onResetLayout} title="Clear saved positions"
            >reset layout</button
          >
          <button class="btn btn-ghost" onclick={onReload} title="Reload from backend, discard draft"
            >reload</button
          >
        </div>
      </div>

      {#if blockingDiags.length}
        <div class="banner banner-err">
          <div class="banner-title">
            save blocked · {blockingDiags.length} type error{blockingDiags.length > 1 ? 's' : ''}
          </div>
          {#each blockingDiags as d (d.nodeId + d.kind + d.message)}
            <div class="banner-msg">{d.message}</div>
            <div class="banner-detail">node {d.nodeId} · {d.kind}</div>
          {/each}
          <div class="banner-foot">fix the flagged node(s), then Save</div>
        </div>
      {/if}
      {#if saveError}
        <div class="banner banner-err">
          <div class="banner-title">invalid edit · {saveError.code}</div>
          <div class="banner-msg">{saveError.message}</div>
          {#if saveError.details?.nodeId}
            <div class="banner-detail">node {saveError.details.nodeId}</div>
          {/if}
          <div class="banner-foot">draft kept — fix it and Save again</div>
        </div>
      {/if}
      {#if saveOk && !saveError}
        <div class="banner banner-ok">{saveOk}</div>
      {/if}
      {#if run.error}
        <div class="banner banner-err">
          <div class="banner-title">run error</div>
          <div class="banner-msg">{run.error}</div>
        </div>
      {/if}

      <DisplayPanel {run} />

      {#if draftDoc}
        <SourceView
          source={canonicalSource}
          version={savedVersion}
          {dirty}
          onProject={projectSupported ? handleProject : undefined}
        />
      {/if}
    </aside>
  </main>
</div>

<style>
  .studio {
    display: flex;
    flex-direction: column;
    height: 100vh;
    background:
      radial-gradient(1200px 600px at 15% -10%, rgba(56, 224, 208, 0.06), transparent 60%),
      radial-gradient(1000px 500px at 100% 0%, rgba(177, 140, 255, 0.06), transparent 55%),
      var(--ink);
  }

  .topbar {
    display: flex;
    align-items: center;
    justify-content: space-between;
    gap: 20px;
    padding: 12px 22px;
    border-bottom: 1px solid var(--line);
    background: rgba(10, 13, 19, 0.7);
    backdrop-filter: blur(8px);
    z-index: 5;
  }
  .brand {
    display: flex;
    align-items: center;
    gap: 12px;
  }
  .mark {
    font-size: 26px;
    color: var(--cyan);
    text-shadow: 0 0 18px color-mix(in srgb, var(--cyan) 60%, transparent);
  }
  .brand-title {
    font-weight: 700;
    font-size: 16px;
    letter-spacing: 0.01em;
  }
  .brand-sub {
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--text-faint);
    letter-spacing: 0.05em;
  }

  .topbar-right {
    display: flex;
    align-items: center;
    gap: 18px;
    flex-wrap: wrap;
    justify-content: flex-end;
  }
  .mode-switch {
    display: inline-flex;
    padding: 3px;
    gap: 2px;
    background: var(--ink-2);
    border: 1px solid var(--line-strong);
    border-radius: 10px;
  }
  .mode-btn {
    font-family: var(--font-ui);
    font-size: 12px;
    font-weight: 600;
    color: var(--text-dim);
    background: transparent;
    border: none;
    border-radius: 7px;
    padding: 6px 14px;
    cursor: pointer;
    transition:
      color 0.15s ease,
      background 0.15s ease,
      box-shadow 0.15s ease;
  }
  .mode-btn:hover {
    color: var(--text);
  }
  .mode-btn.is-active {
    color: var(--ink);
    background: linear-gradient(180deg, var(--cyan), #1fbfae);
    box-shadow: 0 4px 14px -6px var(--cyan);
  }

  .legend {
    display: flex;
    flex-wrap: wrap;
    align-items: center;
    gap: 12px;
  }
  .legend.is-hidden {
    display: none;
  }
  .legend-item {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-family: var(--font-mono);
    font-size: 10px;
    color: var(--text-dim);
    letter-spacing: 0.03em;
  }
  .legend-dot {
    width: 9px;
    height: 9px;
    border-radius: 3px;
    background: var(--c);
    box-shadow: 0 0 8px -1px var(--c);
  }
  .legend-sep {
    width: 1px;
    height: 16px;
    background: var(--line-strong);
  }
  .legend-line {
    width: 18px;
    height: 0;
    border-top: 2px solid;
  }
  .legend-line.seq {
    border-color: rgba(150, 170, 205, 0.6);
  }
  .legend-line.data {
    border-top-style: dashed;
    border-color: var(--cyan);
  }

  .body {
    flex: 1;
    display: flex;
    min-height: 0;
  }
  .canvas {
    position: relative;
    flex: 1;
    min-width: 0;
  }
  .canvas-hint {
    position: absolute;
    left: 14px;
    bottom: 12px;
    font-family: var(--font-mono);
    font-size: 9.5px;
    color: var(--text-faint);
    background: rgba(10, 13, 19, 0.6);
    padding: 4px 9px;
    border-radius: 6px;
    border: 1px solid var(--line);
    pointer-events: none;
    max-width: 60%;
  }
  .palette {
    position: absolute;
    top: 14px;
    left: 14px;
    z-index: 6;
  }
  .palette-btn {
    display: inline-flex;
    align-items: center;
    gap: 5px;
    font-family: var(--font-mono);
    font-size: 11px;
    letter-spacing: 0.03em;
    color: var(--text);
    background: rgba(10, 13, 19, 0.82);
    border: 1px dashed color-mix(in srgb, var(--cyan) 50%, var(--line-strong));
    border-radius: 9px;
    padding: 7px 12px;
    cursor: pointer;
    backdrop-filter: blur(6px);
    transition:
      border-color 0.15s ease,
      background 0.15s ease;
  }
  .palette-btn:hover,
  .palette-btn.is-open {
    border-style: solid;
    border-color: var(--cyan);
    background: rgba(14, 18, 26, 0.92);
  }
  .palette-scope {
    color: var(--text-faint);
  }
  .palette-menu {
    margin-top: 6px;
    display: flex;
    flex-direction: column;
    gap: 2px;
    padding: 6px;
    min-width: 210px;
    max-height: 60vh;
    overflow-y: auto;
    background: var(--ink-2);
    border: 1px solid var(--line-strong);
    border-radius: 11px;
    box-shadow: 0 16px 38px -14px rgba(0, 0, 0, 0.72);
  }
  .palette-group {
    font-family: var(--font-mono);
    font-size: 8px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--text-faint);
    padding: 5px 8px 2px;
  }
  .palette-empty {
    font-family: var(--font-mono);
    font-size: 10px;
    line-height: 1.5;
    color: var(--text-faint);
    padding: 10px 8px;
    text-align: center;
  }
  .palette-item {
    display: inline-flex;
    align-items: center;
    gap: 6px;
    font-family: var(--font-mono);
    font-size: 11px;
    color: var(--text-dim);
    background: transparent;
    border: 1px solid transparent;
    border-radius: 8px;
    padding: 6px 8px;
    cursor: pointer;
    text-align: left;
    transition:
      color 0.12s ease,
      background 0.12s ease,
      border-color 0.12s ease;
  }
  .palette-item:hover {
    color: var(--text);
    background: color-mix(in srgb, var(--c) 14%, transparent);
    border-color: color-mix(in srgb, var(--c) 45%, transparent);
  }
  .palette-glyph {
    color: var(--c);
    font-size: 12px;
    width: 14px;
    text-align: center;
    flex-shrink: 0;
  }

  .overlay-msg {
    position: absolute;
    inset: 0;
    display: flex;
    flex-direction: column;
    gap: 14px;
    align-items: center;
    justify-content: center;
    font-family: var(--font-mono);
    font-size: 13px;
    color: var(--text-dim);
    text-align: center;
  }
  .overlay-msg.error {
    color: var(--rose);
  }
  .overlay-msg .mono {
    font-size: 10px;
    color: var(--text-faint);
  }

  .side {
    width: 418px;
    flex-shrink: 0;
    border-left: 1px solid var(--line);
    background: rgba(10, 13, 19, 0.5);
    padding: 16px;
    overflow-y: auto;
    display: flex;
    flex-direction: column;
    gap: 14px;
  }

  .wf-select {
    display: flex;
    flex-direction: column;
    gap: 6px;
  }
  .wf-select-label {
    font-family: var(--font-mono);
    font-size: 9.5px;
    letter-spacing: 0.14em;
    text-transform: uppercase;
    color: var(--text-faint);
  }
  .wf-select-wrap {
    position: relative;
  }
  .wf-select-input {
    appearance: none;
    width: 100%;
    background: linear-gradient(180deg, var(--node-hi), var(--node));
    border: 1px solid var(--line-strong);
    border-radius: 10px;
    color: var(--text);
    font-family: var(--font-ui);
    font-weight: 600;
    font-size: 14px;
    padding: 11px 34px 11px 13px;
    cursor: pointer;
    transition:
      border-color 0.2s ease,
      box-shadow 0.2s ease;
  }
  .wf-select-input:hover:not(:disabled) {
    border-color: color-mix(in srgb, var(--cyan) 45%, var(--line-strong));
  }
  .wf-select-input:focus {
    outline: none;
    border-color: var(--cyan);
    box-shadow: 0 0 0 3px color-mix(in srgb, var(--cyan) 14%, transparent);
  }
  .wf-select-input:disabled {
    opacity: 0.55;
    cursor: wait;
  }
  .wf-select-input option {
    background: var(--ink-2);
    color: var(--text);
  }
  .wf-select-caret {
    position: absolute;
    right: 13px;
    top: 50%;
    transform: translateY(-50%);
    color: var(--cyan);
    font-size: 11px;
    pointer-events: none;
  }
  .wf-select-desc {
    font-family: var(--font-mono);
    font-size: 10px;
    line-height: 1.45;
    color: var(--text-faint);
    min-height: 14px;
  }

  .controls {
    display: grid;
    grid-template-columns: 1fr 1fr;
    gap: 10px;
  }
  .ctrl-minor {
    grid-column: 1 / -1;
    display: flex;
    flex-wrap: wrap;
    gap: 10px;
  }
  .btn {
    border: 1px solid var(--line-strong);
    background: var(--ink-3);
    color: var(--text);
    font-size: 13px;
    font-weight: 600;
    padding: 11px 14px;
    border-radius: 10px;
    transition:
      transform 0.12s ease,
      background 0.2s ease,
      box-shadow 0.2s ease,
      border-color 0.2s ease;
  }
  .btn:hover:not(:disabled) {
    transform: translateY(-1px);
  }
  .btn:active:not(:disabled) {
    transform: translateY(0);
  }
  .btn:disabled {
    opacity: 0.45;
    cursor: not-allowed;
  }
  .btn-glyph {
    font-size: 11px;
    margin-right: 4px;
  }
  .btn-play {
    background: linear-gradient(180deg, #1c8f83, #147268);
    border-color: color-mix(in srgb, var(--cyan) 50%, transparent);
    color: #eafffb;
  }
  .btn-play:hover:not(:disabled) {
    box-shadow: 0 8px 24px -10px var(--cyan);
  }
  .btn-save {
    background: linear-gradient(180deg, #2b3247, #1d2333);
    border-color: var(--line-strong);
  }
  .btn-save:hover:not(:disabled) {
    box-shadow: 0 8px 22px -12px rgba(120, 150, 200, 0.6);
  }
  .btn-save.is-blocked {
    border-color: color-mix(in srgb, var(--rose) 55%, var(--line-strong));
    color: #ffb4c6;
    opacity: 1;
  }
  .btn-ghost {
    flex: 1 1 40%;
    font-size: 11px;
    font-weight: 500;
    padding: 8px 10px;
    background: transparent;
    color: var(--text-dim);
    border-style: dashed;
  }
  .btn-ghost:hover:not(:disabled) {
    color: var(--text);
    background: var(--ink-3);
  }

  .banner {
    border-radius: 11px;
    padding: 11px 13px;
    font-size: 12px;
  }
  .banner-err {
    background: color-mix(in srgb, var(--rose) 12%, var(--ink-2));
    border: 1px solid color-mix(in srgb, var(--rose) 40%, transparent);
  }
  .banner-title {
    font-family: var(--font-mono);
    font-size: 10px;
    text-transform: uppercase;
    letter-spacing: 0.08em;
    color: var(--rose);
    margin-bottom: 4px;
  }
  .banner-msg {
    color: var(--text);
    line-height: 1.4;
  }
  .banner-detail,
  .banner-foot {
    font-family: var(--font-mono);
    font-size: 9.5px;
    color: var(--text-faint);
    margin-top: 5px;
  }
  .banner-ok {
    background: rgba(95, 208, 138, 0.12);
    border: 1px solid rgba(95, 208, 138, 0.4);
    color: #9fe6bb;
    font-family: var(--font-mono);
    font-size: 11px;
    text-align: center;
  }

  @media (max-width: 900px) {
    .body {
      flex-direction: column;
    }
    .side {
      width: 100%;
      border-left: none;
      border-top: 1px solid var(--line);
      max-height: 48vh;
    }
  }
</style>
