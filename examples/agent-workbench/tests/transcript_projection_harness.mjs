import assert from 'node:assert/strict';
import vm from 'node:vm';
import { createFakeDocument } from './fake_dom.mjs';

export const SURFACES = ['workbench'];

function block(asset, begin, end) {
  assert.equal(asset.split(begin).length, 2, `missing production marker ${begin}`);
  assert.equal(asset.split(end).length, 2, `missing production marker ${end}`);
  return asset.split(begin)[1].split(end)[0];
}

const attachmentOf = attachment => ({attachment_id: attachment.id, retrieve_url: `/api/attachments/${encodeURIComponent(attachment.id)}`});
const galleryOf = node => (node.querySelector('.message-attachments')?.children || [])
  .map(link => ({attachment_id: link.dataset.attachmentId, retrieve_url: link.href}));
/* A tool's declared display as a reader sees it; `null` when it declared
   none. */
const displayOf = tool => {
  const display = tool.querySelector('.tool-display');
  if (!display) return null;
  const summary = tool.querySelector('.tool-summary');
  return {arguments: display.querySelector('.tool-arguments')?.textContent ?? null,
    result: summary.dataset.declared === 'result' ? summary.textContent : null,
    links: (display.querySelector('.tool-links')?.children || []).map(link => ({uri: link.href, title: link.dataset.title ?? null})),
    truncated: Boolean(display.querySelector('.tool-truncated'))};
};
const expectedDisplay = display => display ? {arguments: display.arguments ?? null, result: display.result ?? null,
  links: (display.links || []).map(link => ({uri: link.uri, title: link.title ?? null})), truncated: Boolean(display.truncated)} : null;
const displayedTools = node => node.querySelectorAll('.tool').filter(tool => tool.querySelector('.tool-display'))
  .map(tool => ({operation: tool.querySelector('strong').textContent, display: displayOf(tool)}));
const prose = text => String(text ?? '').replace(/&amp;/g, '&').replace(/&lt;/g, '<').replace(/&gt;/g, '>')
  .replace(/&quot;/g, '"').replace(/&#39;/g, "'").replace(/\s+/g, ' ').trim();

/* What a reader sees of one rendered row, read back from the DOM. */
function observe(node) {
  if (node.classList.contains('reasoning')) return {kind: 'reasoning', value: node.querySelector('pre').textContent};
  if (node.classList.contains('code-block')) {
    const tools = node.querySelectorAll('.tool');
    return {kind: 'code', value: {
      label: node.querySelector('span').textContent.split(' ')[0],
      code: node.querySelector('.code-source').textContent,
      output: node.querySelector('.code-output').textContent,
      failed: node.classList.contains('fail'),
      attachments: galleryOf(node),
      tools: tools.filter(tool => !tool.classList.contains('omitted'))
        .map(tool => ({operation: tool.querySelector('strong').textContent, status: JSON.parse(tool.querySelector('pre').textContent).status,
          display: displayOf(tool)})),
      tools_omitted: tools.filter(tool => tool.classList.contains('omitted')).length ? Number(tools.at(-1).textContent.split(' ')[0]) : 0,
    }};
  }
  if (node.classList.contains('event')) {
    const title = node.querySelector('.event-title')?.textContent || '';
    const detail = node.querySelector('.msg-body').children.find(child => child.classList.contains('payload'))?.querySelector('pre').textContent;
    return {kind: 'message', value: {text: prose(detail ? `${title}\n${detail}` : title), attachments: galleryOf(node), tools: displayedTools(node)}};
  }
  return {kind: 'message', value: {text: prose(node.querySelector('.msg-text').textContent), attachments: galleryOf(node), tools: displayedTools(node)}};
}

// The production timeline module runs unchanged over a tree DOM; what it
// rendered is read back from that DOM. Expected values are taken directly from
// the supplied records.
export function verifyTranscriptSurface(surface, asset, rows, displayOrder = rows.filter(row => !row.suppressed).map(row => row.row_id)) {
  if (surface !== 'workbench') throw new Error(`unregistered surface ${surface}`);
  block(asset, '// BEGIN WORKBENCH_SETTLED_TRANSCRIPT', '// END WORKBENCH_SETTLED_TRANSCRIPT');
  const document = createFakeDocument();
  const context = {document, setTimeout, clearTimeout};
  vm.runInNewContext(asset, context);
  const list = document.createElement('div');
  const footer = document.createElement('div');
  const empty = document.createElement('div');
  document.body.append(list, footer, empty);
  const timeline = context.createWorkbenchTimeline({list, footer, empty});
  // The UI owns its input row under an identity of its own; the committed
  // input of the same turn is that row, by typed turn provenance.
  const input = rows.find(row => !row.suppressed && row.kind === 'user' && row.provenance.turn_id);
  const ownedInput = input ? {id: 'unrelated-ui-identity', role: 'user', text: input.content.text, at: input.timestamp,
    attachments: input.content.attachments.map(attachmentOf),
    provenance: {kind: 'turn_input', turn_id: input.provenance.turn_id}} : null;
  timeline.applySnapshot({transcript: rows, product_events: {cursor: 1, events: ownedInput ? [{event_id: 'owned', type: 'message', message: ownedInput}] : []},
    active_turns: [], pending_turn_inputs: [], turn_input_applications: [], unknown_turn_terminals: []});
  const rendered = list.children.filter(node => !node.hidden);
  const observed = rendered.map(observe);
  const expected = [];
  const expectedIds = [];
  const visibleRows = rows.filter(row => !row.suppressed);
  assert.deepEqual([...displayOrder].sort(), visibleRows.map(row => row.row_id).sort(), 'display order must retain every canonical row once');
  for (const id of displayOrder) {
    const row = visibleRows.find(row => row.row_id === id);
    const content = row.content;
    for (const text of content.reasoning) { expected.push({kind: 'reasoning', value: text}); expectedIds.push(row.row_id); }
    if (row.kind === 'code_block') {
      expected.push({kind: 'code', value: {label: content.language || 'code', code: content.code || '',
        output: [content.output, content.error].filter(Boolean).join('\n'), failed: Boolean(content.error),
        attachments: content.attachments.map(attachmentOf),
        tools: content.tools.map(tool => ({operation: tool.operation, status: tool.status, display: expectedDisplay(tool.display)})),
        tools_omitted: content.tools_omitted}});
      expectedIds.push(row.row_id);
    } else if (row.kind !== 'reasoning') {
      expected.push({kind: 'message', value: {text: prose(content.text), attachments: content.attachments.map(attachmentOf),
        tools: content.tools.filter(tool => tool.display).map(tool => ({operation: tool.operation, display: expectedDisplay(tool.display)}))}});
      expectedIds.push(row.row_id);
    }
  }
  assert.deepEqual(JSON.parse(JSON.stringify(observed)), JSON.parse(JSON.stringify(expected)), `${surface}: canonical content changed`);
  assert.deepEqual(rendered.map(node => node.dataset.transcriptRowId), expectedIds, `${surface}: rows were dropped, duplicated or rendered out of lane order`);
  assert.deepEqual(rendered.map(node => node.dataset.turnId), expectedIds.map(id => rows.find(row => row.row_id === id).provenance.turn_id || ''), `${surface}: typed provenance changed`);
  if (ownedInput) {
    const inputs = rendered.filter(node => node.dataset.turnId === ownedInput.provenance.turn_id && node.classList.contains('user'));
    assert.equal(inputs.length, rows.filter(row => !row.suppressed && row.kind === 'user' && row.provenance.turn_id === ownedInput.provenance.turn_id).length,
      'UI input correlation must render its committed input once');
  }
  return {surface, visible: rows.filter(row => !row.suppressed).length, pieces: expected.length};
}

export function allKindRecords() {
  return ['user', 'assistant_reply', 'reasoning', 'tool_call', 'code_block', 'attachment', 'event'].map((kind, index) => ({
    row_id: `opaque-${index}`, kind, timestamp: '2026-08-18T12:34:56Z', suppressed: null,
    provenance: {turn_id: 'live-turn', input_id: 'input', plugin_id: null, is_turn_reply: kind === 'assistant_reply'},
    content: {text: kind === 'reasoning' || kind === 'code_block' ? '' : `content ${index}`,
      reasoning: kind === 'reasoning' ? ['reason one', 'reason two'] : [],
      attachments: kind === 'attachment' ? [{id:'sha256:one'}, {id:'sha256:two'}] : [],
      language: kind === 'code_block' ? 'typescript' : null, code: kind === 'code_block' ? 'print(1)' : null,
      output: kind === 'code_block' ? '1' : null, success: kind === 'code_block' ? true : null, error: null,
      tools: kind === 'code_block' ? [{call_id: null, operation:'board.move', status:'ok', display: null},
          {call_id: 'tc_search', operation:'web.search', status:'success', display: {arguments: 'lash durable sessions',
            result: '2 results', links: [{uri: 'https://example.test/a', title: 'A'}, {uri: 'https://example.test/b'}]}}]
        : kind === 'tool_call' ? [{call_id: 'tc_fetch', operation: 'fetch', status: 'success',
          display: {arguments: 'https://example.test/page', result: 'HTTP 200', truncated: {original_bytes: 20000, limit_bytes: 16384}}}]
        : [],
      tools_omitted: kind === 'code_block' ? 3 : 0},
  }));
}
