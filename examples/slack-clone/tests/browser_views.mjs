import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const html = readFileSync(new URL('../assets/index.html', import.meta.url), 'utf8');
const source = html.match(/<script type="module">([\s\S]*?)<\/script>/)[1];
const A = { id: 'A', name: 'alpha' };
const B = { id: 'B', name: 'beta' };
const message = (ts, extra = {}) => ({
  type: 'message', ts, text: ts, author_name: 'Ada', ...extra,
});
const deferred = () => {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
};
const flush = async () => {
  for (let i = 0; i < 12; i++) await Promise.resolve();
};

class Element {
  constructor() {
    this.children = [];
    this.dataset = {};
    this.listeners = new Map();
    this.classes = new Set();
    this.classList = {
      add: (value) => this.classes.add(value),
      remove: (value) => this.classes.delete(value),
    };
    this.hidden = false;
    this.value = '';
    this.focusCount = 0;
  }
  append(...children) {
    for (const child of children) {
      if (typeof child === 'object') child.parent = this;
      this.children.push(child);
    }
  }
  replaceChildren(...children) { this.children = []; this.append(...children); }
  remove() { this.parent.children = this.parent.children.filter((child) => child !== this); }
  get lastElementChild() { return this.children.at(-1); }
  setAttribute(name, value) { this[name] = value; }
  addEventListener(name, callback) { this.listeners.set(name, callback); }
  focus() { this.focusCount++; }
  showModal() {}
  close() {}
  querySelector(selector) {
    const matches = (child) => {
      if (!(child instanceof Element)) return false;
      if (selector === '.empty' || selector === '.thread-badge') {
        return child.className === selector.slice(1);
      }
      const ts = selector.match(/^\.msg\[data-ts="(.*)"\]$/)?.[1];
      return ts !== undefined && child.dataset.ts === ts;
    };
    for (const child of this.children) {
      if (matches(child)) return child;
      const nested = child.querySelector?.(selector);
      if (nested) return nested;
    }
    return null;
  }
}

async function client() {
  const elements = new Map();
  const el = (id) => {
    if (!elements.has(id)) elements.set(id, new Element());
    return elements.get(id);
  };
  el('error').hidden = true;
  el('threadPanel').hidden = true;
  const requests = [];
  const timers = [];
  const fetch = (path, options = {}) => {
    if (path === '/platform/bootstrap') {
      return Promise.resolve({ ok: true, json: async () => ({
        identity: { bot_user_id: 'BOT' }, channels: [A, B], users: [],
      }) });
    }
    const response = deferred();
    const request = { path, options, response };
    requests.push(request);
    // Deliberately deliver queued completions even after abort.
    return response.promise;
  };
  const context = vm.createContext({
    document: { getElementById: el, createElement: () => new Element(),
      createDocumentFragment: () => new Element() },
    fetch, AbortController, TextDecoder, CSS: { escape: (value) => value },
    localStorage: { getItem: () => null, setItem() {} },
    setTimeout: (callback, delay) => {
      const timer = { callback, delay, cancelled: false };
      timers.push(timer);
      return timer;
    },
    clearTimeout: (timer) => { if (timer) timer.cancelled = true; },
  });
  await vm.runInContext(`(async () => { ${source}\n globalThis.client = { selectChannel, openThread, closeThread, state }; })()`, context);
  const history = (channel, thread) => requests.findLast((request) =>
    request.path === `/platform/history?channel=${channel}${thread ? `&thread_ts=${thread}` : ''}`);
  const answer = async (request, messages) => {
    request.response.resolve({ ok: true, json: async () => ({ messages }) });
    await flush();
  };
  const fail = async (request, error = 'offline') => {
    request.response.reject(new Error(error));
    await flush();
  };
  const streamRequests = () => requests.filter((request) => request.path.startsWith('/platform/stream'));
  const stream = async (request = streamRequests().at(-1)) => {
    let read = deferred();
    let cancelled = false;
    const reader = { read: () => read.promise, cancel: async () => { cancelled = true; } };
    request.response.resolve({ ok: true, body: { getReader: () => reader } });
    await flush();
    return {
      request,
      get cancelled() { return cancelled; },
      emit: async (text) => {
        const pending = read;
        read = deferred();
        pending.resolve({ done: false, value: new TextEncoder().encode(text) });
        await flush();
      },
      end: async () => { read.resolve({ done: true }); await flush(); },
      fail: async () => { read.reject(new Error('offline')); await flush(); },
    };
  };
  const select = async (channel, messages = []) => {
    const selecting = context.client.selectChannel(channel);
    await answer(history(channel.id), messages);
    await selecting;
  };
  const rows = (id = 'stream') => el(id).children.filter((row) => row.dataset?.ts).map((row) => row.dataset.ts);
  return { ...context.client, el, history, answer, fail, select, stream, streamRequests, timers, rows, requests };
}

for (const order of ['AB', 'BA']) {
  test(`channel history ${order} publishes only under the selected heading and subscription`, async () => {
    const c = await client();
    const a = c.selectChannel(A);
    const old = c.history('A');
    const b = c.selectChannel(B);
    const current = c.history('B');
    for (const id of order) await c.answer(id === 'A' ? old : current, [message(id === 'A' ? '1.0' : '2.0')]);
    await Promise.all([a, b]);
    assert.equal(c.el('channelName').textContent, '# beta');
    assert.deepEqual(c.rows(), ['2.0']);
    assert.deepEqual(c.streamRequests().map((request) => request.path), ['/platform/stream?channel=B']);
    assert.equal(old.options.signal?.aborted, true);
  });
}

test('selection immediately cancels the old stream and ignores its queued read', async () => {
  const c = await client();
  await c.select(A, [message('1.0')]);
  const old = await c.stream();
  const selecting = c.selectChannel(B);
  assert.equal(old.request.options.signal.aborted, true);
  await old.emit(`${JSON.stringify(message('3.0'))}\n`);
  assert.deepEqual(c.rows(), []);
  await c.answer(c.history('B'), [message('2.0')]);
  await selecting;
  assert.deepEqual(c.rows(), ['2.0']);
  assert.equal(c.el('error').hidden, true);
});

test('a delayed old stream response cannot publish or reconnect', async () => {
  const c = await client();
  await c.select(A);
  const oldRequest = c.streamRequests().at(-1);
  await c.select(B, [message('2.0')]);
  const old = await c.stream(oldRequest);
  await old.emit(`${JSON.stringify(message('1.0'))}\n`);
  await old.end();
  assert.deepEqual(c.rows(), ['2.0']);
  assert.equal(c.timers.length, 0);
  assert.equal(old.cancelled, true);
});

for (const ending of ['end', 'fail']) {
  test(`A B A invalidates the old ${ending} reconnect timer`, async () => {
    const c = await client();
    await c.select(A);
    const old = await c.stream();
    await old[ending]();
    const timer = c.timers.at(-1);
    assert.ok(timer);
    await c.select(B);
    await c.select(A, [message('3.0')]);
    const current = c.streamRequests().at(-1);
    const count = c.streamRequests().length;
    timer.callback(); // A callback can already be queued when cleared.
    await flush();
    assert.equal(c.streamRequests().length, count);
    assert.equal(current.options.signal.aborted, false);
    assert.equal(timer.cancelled, true);
    assert.deepEqual(c.rows(), ['3.0']);
    assert.equal(c.el('error').hidden, true);
  });
}

for (const order of ['AB', 'BA']) {
  test(`thread history ${order} publishes and focuses only the current thread`, async () => {
    const c = await client();
    await c.select(A);
    const a = c.openThread('1.0');
    const old = c.history('A', '1.0');
    const b = c.openThread('2.0');
    const current = c.history('A', '2.0');
    for (const id of order) await c.answer(id === 'A' ? old : current, [message(id === 'A' ? '1.0' : '2.0')]);
    await Promise.all([a, b]);
    assert.deepEqual(c.rows('threadStream'), ['2.0']);
    assert.equal(c.el('threadText').focusCount, 1);
    assert.equal(c.el('threadPanel').hidden, false);
    assert.equal(old.options.signal?.aborted, true);
  });
}

for (const action of ['close', 'switch']) {
  test(`${action} invalidates pending thread history and focus`, async () => {
    const c = await client();
    await c.select(A);
    const opening = c.openThread('1.0');
    const old = c.history('A', '1.0');
    if (action === 'close') c.closeThread();
    else await c.select(B);
    await c.answer(old, [message('1.0')]);
    await opening;
    assert.deepEqual(c.rows('threadStream'), []);
    assert.equal(c.el('threadText').focusCount, 0);
    assert.equal(c.el('threadPanel').hidden, true);
    assert.equal(old.options.signal?.aborted, true);
  });
}

test('stale failed thread history cannot close the replacement panel or show an error', async () => {
  const c = await client();
  await c.select(A);
  const oldOpening = c.openThread('1.0');
  const old = c.history('A', '1.0');
  const currentOpening = c.openThread('2.0');
  await c.answer(c.history('A', '2.0'), [message('2.0')]);
  await currentOpening;
  await c.fail(old);
  await oldOpening;
  assert.equal(c.el('threadPanel').hidden, false);
  assert.deepEqual(c.rows('threadStream'), ['2.0']);
  assert.equal(c.el('error').hidden, true);
});

test('stale failed channel history cannot publish an empty state or start a stream', async () => {
  const c = await client();
  const oldSelecting = c.selectChannel(A);
  const old = c.history('A');
  await c.select(B, [message('2.0')]);
  await c.fail(old);
  await oldSelecting;
  assert.deepEqual(c.rows(), ['2.0']);
  assert.equal(c.el('stream').querySelector('.empty'), null);
  assert.deepEqual(c.streamRequests().map((request) => request.path), ['/platform/stream?channel=B']);
  assert.equal(c.el('error').hidden, true);
});

test('current history and thread errors surface while current stream errors retry', async () => {
  const c = await client();
  const selecting = c.selectChannel(A);
  await c.fail(c.history('A'));
  await selecting;
  assert.match(c.el('error').textContent, /Could not load channel.*offline/);
  const opening = c.openThread('1.0');
  await c.fail(c.history('A', '1.0'));
  await opening;
  assert.match(c.el('error').textContent, /Could not open thread.*offline/);
  assert.equal(c.el('threadPanel').hidden, true);
  const current = await c.stream();
  await current.fail();
  assert.match(c.el('error').textContent, /Live stream dropped.*offline/);
  const count = c.streamRequests().length;
  c.timers.at(-1).callback();
  await flush();
  assert.equal(c.streamRequests().length, count + 1);
  assert.equal(c.streamRequests().at(-1).path, '/platform/stream?channel=A');
});

test('thread broadcasts, deduplication, framing, and submission retain their routes', async () => {
  const c = await client();
  await c.select(A, [message('1.0')]);
  const current = await c.stream();
  const opening = c.openThread('1.0');
  await c.answer(c.history('A', '1.0'), [message('1.0', { thread_ts: '1.0' })]);
  await opening;
  const reply = message('2.0', { thread_ts: '1.0' });
  const broadcast = message('3.0', { thread_ts: '1.0', reply_broadcast: true });
  const chunk = `${JSON.stringify(reply)}\n${JSON.stringify(broadcast)}\n`;
  await current.emit(chunk.slice(0, 10));
  await current.emit(chunk.slice(10));
  await current.emit(`${JSON.stringify(broadcast)}\n`);
  assert.deepEqual(c.rows(), ['1.0', '3.0']);
  assert.deepEqual(c.rows('threadStream'), ['1.0', '2.0', '3.0']);
  c.state.me = { user_id: 'ADA' };
  c.el('threadText').value = 'reply';
  const submitting = c.el('threadComposer').listeners.get('submit')({ preventDefault() {} });
  const post = c.requests.at(-1);
  assert.equal(post.path, '/platform/messages');
  assert.deepEqual(JSON.parse(post.options.body), { channel: 'A', user_id: 'ADA', text: 'reply', thread_ts: '1.0' });
  await c.answer(post, []);
  await submitting;
});

for (const composer of ['composer', 'threadComposer']) {
  test(`${composer} stale submission failure cannot restore text or show an outage in a replacement view`, async () => {
    const c = await client();
    await c.select(A);
    if (composer === 'threadComposer') {
      const opening = c.openThread('1.0');
      await c.answer(c.history('A', '1.0'), []);
      await opening;
    }
    c.state.me = { user_id: 'ADA' };
    const input = c.el(composer === 'composer' ? 'text' : 'threadText');
    input.value = 'old draft';
    const sending = c.el(composer).listeners.get('submit')({ preventDefault() {} });
    const post = c.requests.at(-1);
    await c.select(B);
    input.value = 'new draft';
    await c.fail(post);
    await sending;
    assert.equal(input.value, 'new draft');
    assert.equal(c.el('error').hidden, true);
  });
}

test('a queued old stream error stays quiet during replacement history', async () => {
  const c = await client();
  await c.select(A);
  const old = await c.stream();
  const selecting = c.selectChannel(B);
  await old.fail();
  assert.equal(c.el('error').hidden, true);
  assert.equal(c.timers.length, 0);
  await c.answer(c.history('B'), []);
  await selecting;
});

test('closing and reopening the same thread invalidates its old generation', async () => {
  const c = await client();
  await c.select(A);
  const oldOpening = c.openThread('1.0');
  const old = c.history('A', '1.0');
  c.closeThread();
  const opening = c.openThread('1.0');
  await c.answer(c.history('A', '1.0'), [message('2.0')]);
  await opening;
  await c.answer(old, [message('1.0')]);
  await oldOpening;
  assert.deepEqual(c.rows('threadStream'), ['2.0']);
  assert.equal(c.el('threadText').focusCount, 1);
});
