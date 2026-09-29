// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from 'vitest';
import { flushSync, mount, unmount } from 'svelte';
import App from '../App.svelte';

const api = vi.hoisted(() =>
  Object.fromEntries(
    [
      'fetchWorkflow',
      'fetchWorkflows',
      'fetchOperations',
      'selectWorkflow',
      'saveWorkflow',
      'projectSource',
      'runWorkflow',
    ].map((name) => [name, vi.fn()]),
  ),
);
const graph = vi.hoisted(() => ({ buildFlow: vi.fn() }));
vi.mock('./api.js', () => api);
vi.mock('./graph.js', async (original) => ({
  ...(await original()),
  ...graph,
}));
vi.mock('@xyflow/svelte', async () => {
  const { default: Stub } = await import('../test/FlowStub.svelte');
  return {
    SvelteFlow: Stub,
    Background: Stub,
    Controls: Stub,
    MiniMap: Stub,
    BackgroundVariant: { Dots: 'dots' },
  };
});

vi.mock(
  '../components/nodes/WorkflowNode.svelte',
  () => import('../test/FlowStub.svelte'),
);
vi.mock(
  '../components/nodes/ContainerNode.svelte',
  () => import('../test/FlowStub.svelte'),
);
vi.mock(
  '../components/nodes/OpaqueNode.svelte',
  () => import('../test/FlowStub.svelte'),
);
vi.mock(
  '../components/steps/StepsView.svelte',
  () => import('../test/FlowStub.svelte'),
);

function documentFor(source, definition = 'saved-definition') {
  return {
    source,
    definition,
    version: 1,
    nodes: [],
    roots: { main: [], processes: [] },
  };
}
function deferred() {
  let resolve;
  let reject;
  const promise = new Promise((yes, no) => {
    resolve = yes;
    reject = no;
  });
  return { promise, resolve, reject };
}
async function settle() {
  for (let i = 0; i < 8; i += 1) {
    await Promise.resolve();
    flushSync();
  }
}
let app;
let target;
let requests;
function editor() {
  return target.querySelector('textarea.src-input');
}
function draft() {
  return graph.buildFlow.mock.lastCall[0];
}
function input(text) {
  const field = editor();
  field.dispatchEvent(new FocusEvent('focus'));
  field.value = text;
  field.dispatchEvent(new Event('input', { bubbles: true }));
  flushSync();
}
async function start(text) {
  input(text);
  await vi.advanceTimersByTimeAsync(500);
  await settle();
  return requests.at(-1);
}
async function finish(request, source) {
  request.resolve({
    ok: true,
    document: documentFor(source, 'projected-definition'),
  });
  await settle();
}
async function select(id) {
  const picker = target.querySelector('#wf-picker');
  picker.value = id;
  picker.dispatchEvent(new Event('change', { bubbles: true }));
  await settle();
}
async function click(label) {
  const button = [...target.querySelectorAll('button')].find((b) =>
    b.textContent.trim().endsWith(label),
  );
  expect(button, label).toBeTruthy();
  button.click();
  await settle();
}
beforeEach(async () => {
  vi.useFakeTimers();
  vi.clearAllMocks();
  const storage = new Map();
  vi.stubGlobal('localStorage', {
    getItem: (key) => storage.get(key) ?? null,
    setItem: (key, value) => storage.set(key, value),
  });
  localStorage.setItem('lash.wfgraph.mode.v1', 'power');
  localStorage.setItem('lash.wfgraph.view.v1', 'canvas');
  requests = [];
  api.fetchWorkflow.mockResolvedValue(documentFor('initial'));
  api.fetchWorkflows.mockResolvedValue([
    { id: 'onboarding', name: 'First' },
    { id: 'other', name: 'Other' },
  ]);
  api.fetchOperations.mockResolvedValue([]);
  api.selectWorkflow.mockImplementation(async (id) => documentFor(id));
  api.projectSource.mockImplementation(() => {
    const request = deferred();
    requests.push(request);
    return request.promise;
  });
  api.projectSource.mockResolvedValueOnce({ ok: true }); // Capability probe.
  graph.buildFlow.mockImplementation(() => ({ flowNodes: [], flowEdges: [] }));
  target = document.createElement('div');
  document.body.append(target);
  app = mount(App, { target });
  await settle();
  expect(editor()).toBeTruthy();
});
afterEach(async () => {
  if (app) await unmount(app);
  app = null;
  target?.remove();
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe('source projection publication', () => {
  it.each(['old first', 'new first'])(
    'adopts only the latest input when responses finish %s',
    async (order) => {
      const old = await start('old');
      const latest = await start('new');
      if (order === 'old first') {
        await finish(old, 'old');
        expect(draft().source).toBe('initial');
        expect(target.querySelector('.src-dirty')?.textContent).toContain(
          'projecting',
        );
        await finish(latest, 'new');
      } else {
        await finish(latest, 'new');
        await finish(old, 'old');
      }
      expect(draft().source).toBe('new');
      expect(editor().value).toBe('new');
      expect(target.querySelector('.src-error')).toBeNull();
      expect(target.querySelector('.src-dirty')?.textContent).toContain(
        'unsaved',
      );
    },
  );
  it('invalidates an old response at input time before the new debounce fires', async () => {
    const old = await start('old');
    input('still typing');
    await finish(old, 'old');
    expect(requests).toHaveLength(1);
    expect(draft().source).toBe('initial');
    expect(editor().value).toBe('still typing');
    expect(target.querySelector('.src-dirty')?.textContent).toContain(
      'projecting',
    );
  });
  it('ignores an old parse error while the current request is pending', async () => {
    const old = await start('old');
    const latest = await start('new');
    old.resolve({ ok: false, error: { message: 'old parse error' } });
    await settle();
    expect(target.querySelector('.src-error')).toBeNull();
    expect(target.querySelector('.src-dirty')?.textContent).toContain(
      'projecting',
    );
    await finish(latest, 'new');
  });
  it('ignores an old unsupported verdict', async () => {
    const old = await start('old');
    const latest = await start('new');
    old.resolve({ ok: false, unsupported: true });
    await settle();
    expect(editor()).toBeTruthy();
    expect(target.querySelector('.src-dirty')?.textContent).toContain(
      'projecting',
    );
    await finish(latest, 'new');
    expect(draft().source).toBe('new');
  });
  it('keeps the latest rejected text and error after blur and an old success', async () => {
    const old = await start('old');
    const latest = await start('broken text');
    latest.resolve({ ok: false, error: { message: 'latest parse error' } });
    await settle();
    editor().dispatchEvent(new FocusEvent('blur'));
    await settle();
    await finish(old, 'old');
    expect(draft().source).toBe('initial');
    expect(editor().value).toBe('broken text');
    expect(target.querySelector('.src-error')?.textContent).toBe(
      'latest parse error',
    );
    expect(target.querySelector('.src-dirty')?.textContent ?? '').not.toContain(
      'projecting',
    );
  });
  it('ignores a response after selecting another workflow', async () => {
    const old = await start('old');
    await select('other');
    await finish(old, 'old');
    expect(draft().source).toBe('other');
    expect(editor().value).toBe('other');
    expect(target.querySelector('.src-error')).toBeNull();
  });
  it('invalidates publication immediately when workflow selection starts', async () => {
    const selection = deferred();
    api.selectWorkflow.mockReturnValueOnce(selection.promise);
    const old = await start('old');
    await select('other');
    await finish(old, 'old');
    expect(draft().source).toBe('initial');
    selection.resolve(documentFor('other'));
    await settle();
    expect(draft().source).toBe('other');
  });
  it('treats switching away and back to the same workflow as a new epoch', async () => {
    const old = await start('old');
    await select('other');
    await select('onboarding');
    await finish(old, 'old');
    expect(draft().source).toBe('onboarding');
    expect(editor().value).toBe('onboarding');
  });
  it('invalidates pending projection on canonical save adoption', async () => {
    const old = await start('old');
    api.saveWorkflow.mockResolvedValueOnce({
      ok: true,
      document: documentFor('saved'),
      idMap: null,
    });
    await click('Save');
    await finish(old, 'old');
    expect(draft().source).toBe('saved');
    expect(editor().value).toBe('saved');
    expect(target.querySelector('.src-clean')?.textContent).toContain('saved');
  });
  it('invalidates pending projection on undo and redo adoption', async () => {
    const first = await start('first');
    await finish(first, 'first');
    const old = await start('old');
    await click('undo');
    await finish(old, 'old');
    expect(draft().source).toBe('initial');
    expect(editor().value).toBe('initial');
    const pending = await start('pending');
    await click('redo');
    await finish(pending, 'pending');
    expect(draft().source).toBe('first');
    expect(editor().value).toBe('first');
  });
  it('prevents adoption after component removal', async () => {
    const old = await start('old');
    await unmount(app);
    app = null;
    const calls = graph.buildFlow.mock.calls.length;
    await finish(old, 'old');
    expect(graph.buildFlow).toHaveBeenCalledTimes(calls);
  });
  it('clears a debounced request on component removal', async () => {
    input('pending');
    await unmount(app);
    app = null;
    await vi.advanceTimersByTimeAsync(500);
    expect(requests).toHaveLength(0);
  });
  it('retains rejected input when the latest request throws', async () => {
    const latest = await start('broken transport');
    latest.reject(new Error('response decoding failed'));
    await settle();
    editor().dispatchEvent(new FocusEvent('blur'));
    await settle();
    expect(draft().source).toBe('initial');
    expect(editor().value).toBe('broken transport');
    expect(target.querySelector('.src-error')?.textContent).toBe(
      'response decoding failed',
    );
  });
  it('makes the pane read-only for the latest unsupported verdict', async () => {
    const latest = await start('unsupported');
    latest.resolve({ ok: false, unsupported: true });
    await settle();
    expect(editor()).toBeNull();
    expect(draft().source).toBe('initial');
    expect(target.querySelector('.src-error')).toBeNull();
    expect(target.querySelector('.src-clean')?.textContent).toContain('saved');
  });
  it('invalidates pending projection on reload', async () => {
    const old = await start('old');
    api.fetchWorkflow.mockResolvedValueOnce(documentFor('reloaded'));
    api.projectSource.mockResolvedValueOnce({ ok: true });
    await click('reload');
    await finish(old, 'old');
    expect(draft().source).toBe('reloaded');
    expect(editor().value).toBe('reloaded');
  });
  it('does not abort a run stream when projection ownership is disposed', async () => {
    const stream = deferred();
    let signal;
    api.runWorkflow.mockImplementation(async function* (currentSignal) {
      signal = currentSignal;
      await stream.promise;
    });
    await click('Play');
    const old = await start('old');
    await unmount(app);
    app = null;
    await finish(old, 'old');
    expect(signal.aborted).toBe(false);
    stream.resolve();
    await settle();
  });
  it('keeps run overlays qualified by the saved definition after source projection', async () => {
    const first = await start('first');
    await finish(first, 'first');
    api.runWorkflow.mockImplementation(async function* () {
      for (const definition of ['projected-definition', 'saved-definition']) {
        yield {
          definition,
          runId: 'run',
          workflowVersion: 1,
          nodeId: 'shared-node',
          status: 'started',
          display: { messages: [definition] },
        };
      }
    });
    await click('Play');
    expect(target.querySelector('.messages')?.textContent).toContain(
      'saved-definition',
    );
    expect(target.querySelector('.messages')?.textContent).not.toContain(
      'projected-definition',
    );
    expect(target.querySelector('.run-ev')?.textContent).toContain('1 evt');
  });
});
