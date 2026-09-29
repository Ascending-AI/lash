import assert from 'node:assert/strict';
import { readFileSync } from 'node:fs';
import { test } from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('../src/ui.rs', import.meta.url), 'utf8')
  .match(/<script>([\s\S]*?)<\/script>/)[1];

class Element {
  children = [];
  selectors = new Map();
  textContent = '';
  value = '';
  disabled = false;
  dataset = {};
  classList = { toggle() {} };
  scrollHeight = 0;
  set innerHTML(value) {
    this.children = [];
    this.selectors = new Map();
    if (value) this.children.push(new Element(), new Element());
  }
  get lastElementChild() { return this.children.at(-1); }
  appendChild(child) { this.children.push(child); return child; }
  querySelector(selector) {
    if (!this.selectors.has(selector)) this.selectors.set(selector, new Element());
    return this.selectors.get(selector);
  }
  addEventListener() {}
}

function deferred() {
  let resolve, reject;
  const promise = new Promise((yes, no) => { resolve = yes; reject = no; });
  return { promise, resolve, reject };
}
const board = { cells: ['X', 'O', null, null, null, null, null, null, null], turn: 'X' };
const tool = {
  type: 'tool_call_completed', phase: 'completed', name: 'play_move', call_id: 'move',
  success: true, duration_ms: 1, result: { accepted: true, move: { cell: 1 }, board },
};
const message = (text, snapshot) => ({ role: 'assistant', text, payload: snapshot ? { board: snapshot } : {} });

function controller() {
  const elements = new Map();
  const requests = [];
  const alerts = [];
  const context = vm.createContext({
    document: {
      querySelector(selector) {
        if (!elements.has(selector)) elements.set(selector, new Element());
        return elements.get(selector);
      },
      createElement() { return new Element(); },
    },
    TextDecoder, Map, Set, alert: (text) => alerts.push(text),
    fetch(url, options) {
      const response = deferred();
      requests.push({ url, options, ...response });
      return response.promise;
    },
  });
  // Startup remains pending so each fixture controls every production request.
  vm.runInContext(source, context);
  const evaluate = (code) => vm.runInContext(code, context);
  evaluate(`chats = ['A', 'B', 'C'].map(id => ({id, title:id, model_label:'test'})); activeChat = 'A'; renderChats();`);
  const read = (code) => JSON.parse(JSON.stringify(evaluate(code)));
  const request = (url, method) => {
    const found = requests.find(item => !item.claimed && item.url === url && (!method || item.options?.method === method));
    assert.ok(found, `missing request ${method || 'GET'} ${url}`);
    found.claimed = true;
    return found;
  };
  const reply = (url, body) => request(url).resolve({ ok: true, json: async () => body });
  const select = (id) => {
    const buttons = elements.get('#chats').children;
    return buttons[['A', 'B', 'C'].indexOf(id)].onclick();
  };
  const history = async (id, messages = [], points = []) => {
    reply(`/api/chats/${id}/messages`, messages);
    await tick();
    reply(`/api/chats/${id}/branch-points`, points);
    await tick();
  };
  const transcript = () => {
    function content(el) {
      return [el.textContent, ...el.children.map(content), ...[...el.selectors.values()].map(content)].join(' ');
    }
    return content(elements.get('#messages'));
  };
  const stream = () => {
    const reads = [];
    let released = false;
    const reader = {
      read() { const read = deferred(); reads.push(read); return read.promise; },
      releaseLock() { released = true; },
    };
    request('/api/chats/A/messages', 'POST').resolve({
      ok: true,
      headers: { get: () => JSON.stringify({ negotiation: 'accept', selected: 100, supported: { min: 100, max: 100 } }) },
      body: { getReader: () => reader },
    });
    return {
      async items(...items) {
        await tick();
        assert.ok(reads.length, 'stream reader must be waiting');
        reads.shift().resolve({ value: new TextEncoder().encode(items.map(item => JSON.stringify(item)).join('\n') + '\n'), done: false });
        await tick();
      },
      async end() { await tick(); reads.shift().resolve({ done: true }); await tick(); },
      async fail() { await tick(); reads.shift().reject(new Error('reader failed')); await tick(); },
      async malformed() { await tick(); reads.shift().resolve({ value: new TextEncoder().encode('{bad json}\n'), done: false }); await tick(); },
      released: () => released,
    };
  };
  return { evaluate, read, request, reply, select, history, transcript, stream, elements, alerts, requests };
}
async function tick() { await new Promise(resolve => setImmediate(resolve)); }
const observation = activity => ({ type: 'observation', event: { type: 'turn_activity', activity } });

async function finishVisible(h, stream, send) {
  await stream.end();
  h.reply('/api/chats', [{ id: 'A', title: 'A', model_label: 'test' }, { id: 'B', title: 'B', model_label: 'test' }]);
  await tick();
  const pending = h.requests.find(item => !item.claimed && item.url === '/api/chats/A/messages' && item.options?.method !== 'POST');
  if (pending) await h.history('A');
  await send;
}

test('late A stream cannot publish text reasoning tools or board into B', async () => {
  const h = controller();
  const send = h.evaluate("sendText('A turn')");
  const stream = h.stream();
  await stream.items(observation({ type: 'reasoning_delta', text: 'A thinking' }));
  const selection = h.select('B');
  await h.history('B', [message('B history')]);
  await selection;
  await stream.items(
    observation({ type: 'code_block_started', code: 'A code' }), observation(tool),
    observation({ type: 'code_block_completed', success: true, tool_call_ids: ['move'] }),
    observation({ type: 'assistant_prose_delta', text: 'A late prose' }),
    { type: 'message', message: { kind: 'reasoning', text: 'A persisted thinking' } },
    { type: 'message', message: message('A committed prose', board) },
  );
  assert.match(h.transcript(), /B history/);
  assert.doesNotMatch(h.transcript(), /A |play_move/);
  assert.deepEqual(h.read("boards.get('B').cells"), Array(9).fill(null));
  assert.deepEqual(h.read("boards.get('A').cells"), board.cells);
  await finishVisible(h, stream, send);
  assert.equal(stream.released(), true);
  const reopen = h.select('A');
  await h.history('A', [message('A durable history', board)]);
  await reopen;
  assert.match(h.transcript(), /A durable history/);
  assert.deepEqual(h.read('currentBoard().cells'), board.cells);
});

test('late A tool stores its board under A while B remains selected', async () => {
  const h = controller();
  const send = h.evaluate("sendText('A turn')");
  const stream = h.stream();
  const selection = h.select('B');
  await h.history('B');
  await selection;
  await stream.items(observation(tool));
  assert.deepEqual(h.read("boards.get('B').cells"), Array(9).fill(null));
  assert.deepEqual(h.read("boards.get('A').cells"), board.cells);
  await finishVisible(h, stream, send);
});

test('reverse history responses cannot replace the selected transcript or board', async () => {
  const h = controller();
  const b = h.select('B');
  const c = h.select('C');
  await h.history('C', [message('C history')]);
  await c;
  h.reply('/api/chats/B/messages', [message('B stale history', board)]);
  await tick();
  const branch = h.requests.find(item => !item.claimed && item.url === '/api/chats/B/branch-points');
  if (branch) h.reply(branch.url, []);
  await b;
  assert.match(h.transcript(), /C history/);
  assert.doesNotMatch(h.transcript(), /B stale/);
  assert.deepEqual(h.read('currentBoard().cells'), Array(9).fill(null));
});

test('reverse branch responses and returning to the same chat respect the view generation', async () => {
  const h = controller();
  const first = h.select('B');
  h.reply('/api/chats/B/messages', []);
  await tick();
  const oldBranches = h.request('/api/chats/B/branch-points');
  const c = h.select('C');
  await h.history('C');
  await c;
  const second = h.select('B');
  await h.history('B', [message('B current history')], [{ node_id: 'new-pin', message_count: 2 }]);
  await second;
  oldBranches.resolve({ ok: true, json: async () => [{ node_id: 'old-pin', message_count: 1 }] });
  await first;
  assert.equal(h.read('branchPoints[0].node_id'), 'new-pin');
});

for (const failure of ['fetch', 'reader', 'JSON', 'accept JSON', 'negotiation']) {
  test(`${failure} failure releases the matching run and permits another send`, async () => {
    const h = controller();
    const send = h.evaluate("sendText('first turn')");
    const settled = send.catch(() => {});
    let stream;
    if (failure === 'fetch') h.request('/api/chats/A/messages', 'POST').reject(new Error('fetch failed'));
    else if (failure === 'accept JSON' || failure === 'negotiation') {
      h.request('/api/chats/A/messages', 'POST').resolve({ ok: true, headers: { get: () => failure === 'accept JSON' ? '{bad' : '{}' } });
    } else {
      stream = h.stream();
      if (failure === 'reader') await stream.fail(); else await stream.malformed();
    }
    await settled;
    assert.equal(h.elements.get('#resetBoard').disabled, false, `${failure} must release busy state`);
    if (stream) assert.equal(stream.released(), true);
    const retry = h.evaluate("sendText('retry')");
    h.request('/api/chats/A/messages', 'POST').reject(new Error('end retry'));
    await retry.catch(() => {});
  });
}

test('tool code linking reasoning and replay gaps preserve event order', async () => {
  const h = controller();
  const send = h.evaluate("sendText('A turn')");
  const stream = h.stream();
  await stream.items(
    { type: 'replay_cursor', cursor: { sequence: 1 } },
    { type: 'replay_gap', gap: { latest_cursor: { sequence: 5 } } },
    observation({ type: 'reasoning_delta', text: 'thinking delta' }),
    { type: 'message', message: { kind: 'reasoning', text: 'persisted reasoning' } },
    observation({ type: 'code_block_started', code: 'play_move(1)' }),
    observation(tool), observation({ ...tool, call_id: 'unlinked', name: 'read_board' }),
    observation({ type: 'code_block_completed', language: 'js', success: true, tool_call_ids: ['move'] }),
    observation({ type: 'assistant_prose_delta', text: 'answer' }),
  );
  const children = h.elements.get('#messages').children;
  assert.deepEqual(children.map(child => child.className), ['reasoning', 'code-block', 'tool', 'msg assistant']);
  assert.equal(children[1].children.at(-1).className, 'tool');
  assert.equal(children[0].querySelector('pre').textContent, 'persisted reasoning');
  assert.equal(children[1].querySelector('pre').textContent, 'play_move(1)');
  assert.deepEqual(h.read('typeof activeRun === \'undefined\' ? replayCursor : activeRun.replayCursor'), { sequence: 5 });
  await finishVisible(h, stream, send);
  assert.equal(stream.released(), true);
});

test('reloaded code linked tools appear once and retain the durable board', async () => {
  const h = controller();
  const selection = h.select('B');
  await h.history('B', [
    { kind: 'tool_call', payload: tool },
    { kind: 'code_block', payload: { phase: 'completed', success: true, tool_call_ids: ['move'], code: 'saved code' } },
    message('saved answer'),
  ]);
  await selection;
  const children = h.elements.get('#messages').children;
  assert.deepEqual(children.map(child => child.className), ['code-block', 'msg assistant']);
  assert.equal(children[0].children.at(-1).className, 'tool');
  assert.deepEqual(h.read('currentBoard().cells'), board.cells);
});

test('an older completion refresh cannot overwrite a newer run', async () => {
  const h = controller();
  const first = h.evaluate("sendText('first')");
  const stream = h.stream();
  await stream.end();
  const second = h.evaluate("sendText('second')");
  h.reply('/api/chats', [{ id: 'A', title: 'A', model_label: 'test' }]);
  await tick();
  assert.equal(h.elements.get('#resetBoard').disabled, true);
  assert.equal(h.requests.filter(item => item.url === '/api/chats/A/messages' && item.options?.method !== 'POST').length, 0);
  await first;
  h.request('/api/chats/A/messages', 'POST').reject(new Error('end second'));
  await second.catch(() => {});
  assert.equal(h.elements.get('#resetBoard').disabled, false);
});

test('same chat history requests publish only their newest result', async () => {
  const h = controller();
  const first = h.select('B');
  const oldMessages = h.request('/api/chats/B/messages');
  const hasOldBranches = h.requests.some(item => !item.claimed && item.url === '/api/chats/B/branch-points');
  const oldBranches = hasOldBranches ? h.request('/api/chats/B/branch-points') : null;
  const second = h.select('B');
  await h.history('B', [message('B newest')]);
  await second;
  oldMessages.resolve({ ok: true, json: async () => [message('B old', board)] });
  await tick();
  if (oldBranches) oldBranches.resolve({ ok: true, json: async () => [] });
  else h.reply('/api/chats/B/branch-points', []);
  await first;
  assert.match(h.transcript(), /B newest/);
  assert.doesNotMatch(h.transcript(), /B old/);
});

test('returning to A during its turn reloads committed history when it finishes', async () => {
  const h = controller();
  const send = h.evaluate("sendText('A turn')");
  const stream = h.stream();
  const b = h.select('B');
  await h.history('B');
  await b;
  const a = h.select('A');
  // Keep this early snapshot pending until the post-commit history has rendered.
  const earlyHistory = h.request('/api/chats/A/messages');
  if (h.requests.some(item => !item.claimed && item.url === '/api/chats/A/branch-points')) {
    h.reply('/api/chats/A/branch-points', []);
  }
  await stream.items(observation(tool), observation({ type: 'assistant_prose_delta', text: 'A stale view prose' }));
  assert.doesNotMatch(h.transcript(), /A stale view prose/);
  await stream.end();
  h.reply('/api/chats', [{ id: 'A', title: 'A', model_label: 'test' }, { id: 'B', title: 'B', model_label: 'test' }]);
  await tick();
  await h.history('A', [message('A committed durable history', board)]);
  await send;
  earlyHistory.resolve({ ok: true, json: async () => [message('A pre-commit history')] });
  await a;
  assert.match(h.transcript(), /A committed durable history/);
  assert.doesNotMatch(h.transcript(), /A pre-commit history/);
  assert.deepEqual(h.read('currentBoard().cells'), board.cells);
});

test('a reader failure flushes pending tools and releases all turn controls', async () => {
  const h = controller();
  const send = h.evaluate("sendText('A turn')");
  const settled = send.catch(() => {});
  const stream = h.stream();
  await stream.items(observation({ type: 'code_block_started', code: 'interrupted' }), observation(tool));
  await stream.fail();
  await settled;
  assert.deepEqual(h.read('currentBoard().cells'), board.cells);
  assert.match(h.transcript(), /play_move/);
  assert.equal(h.elements.get('#pinBranch').disabled, false);
  assert.equal(h.elements.get('#resetBoard').disabled, false);
  assert.equal(stream.released(), true);
});

test('a fork history failure still reports the error in the selected fork view', async () => {
  const h = controller();
  h.elements.get('#branchPoint').value = 'pin';
  const fork = h.evaluate('forkPinnedTurn()');
  h.request('/api/chats/A/forks', 'POST').resolve({ ok: true, json: async () => ({ id: 'fork', title: 'fork', model_label: 'test' }) });
  await tick();
  h.request('/api/chats/fork/messages').reject(new Error('fork history failed'));
  await fork;
  assert.equal(h.elements.get('#branchStatus').textContent, 'fork history failed');
});
