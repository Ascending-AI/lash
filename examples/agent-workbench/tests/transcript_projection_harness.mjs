import assert from 'node:assert/strict';
import vm from 'node:vm';

export const SURFACES = ['workbench'];

function block(asset, begin, end) {
  assert.equal(asset.split(begin).length, 2, `missing production marker ${begin}`);
  assert.equal(asset.split(end).length, 2, `missing production marker ${end}`);
  return asset.split(begin)[1].split(end)[0];
}
function element(container, payload) {
  const node = { dataset: {}, payload, children: [], appendChild(child) { this.children.push(child); },
    remove() { container.children.splice(container.children.indexOf(this), 1); } };
  container.children.push(node);
  return node;
}

// The production functions run unchanged; presentation calls record the values
// they received. Expected values are taken directly from the supplied records.
export function verifyTranscriptSurface(surface, asset, rows) {
  const target = { children: [] };
  const observed = [];
  const record = (kind, value) => { observed.push({ kind, value }); return element(target, { kind, value }); };
  const owner = { turnId: 'live-turn', pendingTools: [] };
  const context = {
    timeline: target, messagesEl: target,
    isCurrentView: () => true,
    document: { createElement: () => ({ children: [], appendChild(child) { this.children.push(child); } }) },
    appendReasoning: text => record('reasoning', text),
    appendReasoningMessage: text => record('reasoning', text),
    appendCodeBlock: row => record('code', { language:row.language, code:row.code, output:row.output, success:row.success, error:row.error, attachments:row.attachments, tools:row.tools, tools_omitted:row.tools_omitted }),
    renderMessage: message => record('message', { text: message.text, attachments: message.attachments }),
    appendMessage: message => record('message', { text: message.text, attachments: message.attachments }),
  };
  const messageIds = [];
  if (surface === 'workbench') {
    context.renderMessage = message => { messageIds.push(message.id); return record('message', {text:message.text, attachments:message.attachments}); };
  }
  let ownedInput = null;
  let code;
  if (surface === 'workbench') {
    code = block(asset, '// BEGIN WORKBENCH_SETTLED_TRANSCRIPT', '// END WORKBENCH_SETTLED_TRANSCRIPT');
    const input = rows.find(row => !row.suppressed && row.kind === 'user' && row.provenance.turn_id);
    ownedInput = input ? {id:'unrelated-ui-identity', role:'user', text:input.content.text, at:input.timestamp,
      attachments:input.content.attachments.map(attachment => ({attachment_id:attachment.id, retrieve_url:`/api/attachments/${encodeURIComponent(attachment.id)}`})),
      provenance:{kind:'turn_input', turn_id:input.provenance.turn_id}} : null;
    code += `\nrenderStateTranscript({transcript: ${JSON.stringify(rows)}, product_events:{events:${JSON.stringify(ownedInput ? [{message:ownedInput}] : [])}}});`;
  } else throw new Error(`unregistered surface ${surface}`);
  vm.runInNewContext(code, context);
  const expected = [];
  const expectedIds = [];
  for (const row of rows.filter(row => !row.suppressed)) {
    const content = row.content;
    for (const text of content.reasoning) { expected.push({kind:'reasoning', value:text}); expectedIds.push(row.row_id); }
    if (row.kind === 'code_block') {
      expected.push({kind:'code', value:{language:content.language, code:content.code, output:content.output, success:content.success, error:content.error, attachments:content.attachments, tools:content.tools, tools_omitted:content.tools_omitted}});
      expectedIds.push(row.row_id);
    } else if (row.kind !== 'reasoning') {
      const attachments = surface === 'workbench'
        ? content.attachments.map(attachment => ({attachment_id:attachment.id, retrieve_url:`/api/attachments/${encodeURIComponent(attachment.id)}`}))
        : content.attachments;
      expected.push({kind:'message', value:{text:content.text, attachments}});
      expectedIds.push(row.row_id);
    }
  }
  assert.deepEqual(JSON.parse(JSON.stringify(observed)), JSON.parse(JSON.stringify(expected)), `${surface}: canonical content changed`);
  assert.deepEqual(target.children.map(node => node.dataset.transcriptRowId), expectedIds, `${surface}: rows were dropped, duplicated or reordered`);
  assert.deepEqual(target.children.map(node => node.dataset.turnId), expectedIds.map(id => rows.find(row => row.row_id === id).provenance.turn_id || ''), `${surface}: typed provenance changed`);
  if (ownedInput) assert.equal(messageIds.filter(id => id === ownedInput.id).length, 1, 'UI input correlation must preserve its own identity once');
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
      tools: kind === 'code_block' ? [{operation:'board.move', status:'ok'}] : [], tools_omitted: kind === 'code_block' ? 3 : 0},
  }));
}
