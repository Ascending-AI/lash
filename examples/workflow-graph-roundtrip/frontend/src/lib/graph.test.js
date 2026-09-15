import { describe, it, expect } from 'vitest';
import { addNodeToDoc } from './graph.js';

const SHOW_MESSAGE = {
  id: 'display.show_message',
  label: 'Show message',
  nodeKind: 'call',
  operation: 'show_message',
  fields: [{ name: 'text', type: 'string', default: '' }],
};

const SET_PROGRESS = {
  id: 'display.set_progress',
  label: 'Set progress',
  nodeKind: 'call',
  operation: 'set_progress',
  fields: [{ name: 'pct', type: 'number', default: 0 }],
};

// A non-display catalog entry, served with its own receiver and with the
// `$expr` defaults the mocked tools carry (FIG-3178).
const LLM_QUERY = {
  id: 'llm.query',
  label: 'Query LLM',
  nodeKind: 'call',
  operation: 'query',
  receiver: 'llm',
  fields: [
    { name: 'task', type: 'string', default: 'Summarize the supplied input' },
    { name: 'inputs', type: 'expression', default: { $expr: '{}' } },
  ],
};

const SLEEP = {
  id: 'effect.sleep',
  label: 'Sleep',
  nodeKind: 'effect',
  effect: 'sleep',
  fields: [{ name: 'duration', type: 'expression', default: '"1s"' }],
};

const WAIT_SIGNAL = {
  id: 'effect.wait_signal',
  label: 'Wait for signal',
  nodeKind: 'effect',
  effect: 'wait_signal',
  fields: [{ name: 'signal', type: 'string', default: 'continue' }],
};

const IF = {
  id: 'control.if',
  label: 'If / branch',
  nodeKind: 'container',
  subkind: 'if',
  fields: [{ name: 'condition', type: 'expression', default: 'true' }],
};

function blankDoc() {
  return { nodes: [], edges: [], roots: { main: [], processes: [] } };
}

function dataOf(doc, id) {
  return doc.nodes.find((node) => node.id === id)?.data;
}

// FIG-3177: a palette insertion posts `data.expression` verbatim, and the
// backend resolves a call node's operation out of it. An unawaited tool call
// lowers to a pending-tool value with no receiver operation, so the whole save
// is refused; every synthesized call expression must carry `await`.
describe('synthesized call expressions', () => {
  it('awaits the receiver call it seeds from a catalog entry', () => {
    const doc = blankDoc();
    const id = addNodeToDoc(doc, { main: true }, SHOW_MESSAGE);
    expect(dataOf(doc, id).expression).toBe('await display.show_message({ text: "" })');
  });

  it('awaits a numeric-argument call too', () => {
    const doc = blankDoc();
    const id = addNodeToDoc(doc, { main: true }, SET_PROGRESS);
    expect(dataOf(doc, id).expression).toBe('await display.set_progress({ pct: 0 })');
  });

  it('awaits the call seeded into a fresh container slot', () => {
    const doc = blankDoc();
    const containerId = addNodeToDoc(doc, { main: true }, IF, [SHOW_MESSAGE]);
    const seeded = doc.nodes.filter((node) => node.parentId === containerId);
    expect(seeded).toHaveLength(1);
    expect(seeded[0].data.expression).toBe('await display.show_message({ text: "" })');
  });

  // FIG-3178: `list_recent` belongs to `gmail` and `query` to `llm`, so a
  // synthesized `display.<operation>` names a receiver that has no such
  // operation. An expression-valued default arrives as `{ $expr: source }` and
  // used to reach the argument record as the literal `[object Object]`.
  it("names the entry's own receiver and emits $expr defaults raw", () => {
    const doc = blankDoc();
    const id = addNodeToDoc(doc, { main: true }, LLM_QUERY);
    expect(dataOf(doc, id).expression).toBe(
      'await llm.query({ task: "Summarize the supplied input", inputs: {} })'
    );
  });

  it('carries the receiver on the node so a save without an expression matches', () => {
    const doc = blankDoc();
    const id = addNodeToDoc(doc, { main: true }, LLM_QUERY);
    expect(dataOf(doc, id).receiver).toBe('llm');
    expect(dataOf(doc, id).fields.inputs).toEqual({ $expr: '{}' });
  });

  // FIG-3179: a palette insertion is posted before any field is edited, so its
  // seeded expression has to be source the backend's fragment validator
  // accepts. `sleep for "1s"` and `wait_signal("continue")` are the dialect's
  // surface, not TypeScript, so both entries were unsaveable out of the
  // palette; the lens projects these effects as `await sleep(..)` /
  // `await waitSignal(..)`.
  it('seeds effects as the canonical TypeScript the lens projects', () => {
    const doc = blankDoc();
    const sleepId = addNodeToDoc(doc, { main: true }, SLEEP);
    expect(dataOf(doc, sleepId).expression).toBe('await sleep("1s")');
    const waitId = addNodeToDoc(doc, { main: true }, WAIT_SIGNAL);
    expect(dataOf(doc, waitId).expression).toBe('await waitSignal("continue")');
  });

  it('awaits the built-in seed used when the catalog carries no action', () => {
    const doc = blankDoc();
    const containerId = addNodeToDoc(doc, { main: true }, IF, []);
    const seeded = doc.nodes.filter((node) => node.parentId === containerId);
    expect(seeded).toHaveLength(1);
    expect(seeded[0].data.expression).toBe('await display.show_message({ text: "" })');
  });
});
