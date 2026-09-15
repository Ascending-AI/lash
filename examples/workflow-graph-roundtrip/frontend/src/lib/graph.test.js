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

  it('awaits the built-in seed used when the catalog carries no action', () => {
    const doc = blankDoc();
    const containerId = addNodeToDoc(doc, { main: true }, IF, []);
    const seeded = doc.nodes.filter((node) => node.parentId === containerId);
    expect(seeded).toHaveLength(1);
    expect(seeded[0].data.expression).toBe('await display.show_message({ text: "" })');
  });
});
