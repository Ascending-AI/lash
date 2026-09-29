// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from 'vitest';
import { flushSync, mount, unmount } from 'svelte';
import WorkflowNode from '../components/nodes/WorkflowNode.svelte';
import ArgField from '../components/steps/ArgField.svelte';
import { editableKind, editableText, editableSource } from './editableValue.js';
import { fieldDefaultValue, synthCallExpression } from './operations.js';
import { summarizeFieldValue } from './steps.js';
import { layoutDocument } from './layout.js';

vi.mock('@xyflow/svelte', async () => {
  const { default: Stub } = await import('../test/FlowStub.svelte');
  return { Handle: Stub, Position: { Top: 'top', Bottom: 'bottom' } };
});

const literal = { kind: 'object', value: {
  $expr: { kind: 'string', value: '1 + 1' },
  kind: { kind: 'string', value: 'expr' },
  value: { kind: 'list', value: [{ kind: 'object', value: {
    $expr: { kind: 'string', value: 'not valid code!' },
  } }] },
} };
let component;
let target;
afterEach(async () => {
  if (component) await unmount(component);
  target?.remove();
  component = undefined;
});

describe('recursive editable values', () => {
  it('reads the tag and preserves reserved-looking literal keys in summaries and source', () => {
    const expression = { kind: 'expr', value: '1 + 1' };
    expect(editableKind(literal)).toBe('object');
    expect(editableKind(expression)).toBe('expr');
    expect(editableKind({ kind: 'object', value: { $expr: { kind: 'string', value: '1 + 1' } } })).toBe('object');
    expect(editableText(literal)).toBe('{"$expr":"1 + 1","kind":"expr","value":[{"$expr":"not valid code!"}]}');
    expect(summarizeFieldValue(literal)).toBe(editableText(literal));
    expect(summarizeFieldValue(expression)).toBe('1 + 1');
    expect(editableSource(literal)).toBe('{ "$expr": "1 + 1", "kind": "expr", "value": [{ "$expr": "not valid code!" }] }');
    expect(editableSource({ kind: 'list', value: [literal, expression] })).toContain(', 1 + 1]');
    expect(editableSource({ kind: 'null', value: null })).toBe('null');
  });

  it('seeds literal catalog defaults without changing them into expressions', () => {
    const field = { name: 'inputs', type: 'expression', default: literal };
    const copy = fieldDefaultValue(field);
    copy.value.$expr.value = 'edited';
    expect(literal.value.$expr.value).toBe('1 + 1');
    expect(synthCallExpression({ receiver: 'llm', operation: 'query', fields: [field] }))
      .toBe(`await llm.query({ inputs: ${editableSource(literal)} })`);
  });

  it('reserves expression editor height only for expression tags', () => {
    const size = (value) => layoutDocument({
      nodes: [{ id: 'call', type: 'call', data: { kind: 'call', fields: { inputs: value } } }],
      edges: [], roots: { main: ['call'], processes: [] },
    }).sizes.get('call').h;
    expect(size({ kind: 'expr', value: '1 + 1' })).toBe(size(literal) + 10);
  });

  it('keeps nested records read-only in the argument editor through an open attempt', () => {
    target = document.createElement('div');
    document.body.append(target);
    const node = { data: { fields: { inputs: structuredClone(literal) } } };
    component = mount(ArgField, { target, props: { node, fieldKey: 'inputs' } });
    flushSync();
    expect(target.textContent).toContain('"$expr":"1 + 1"');
    target.querySelector('button').click();
    flushSync();
    expect(target.querySelector('input, textarea, select')).toBeNull();
    expect(node.data.fields.inputs).toEqual(literal);
  });

  it('renders literal records on the canvas without an expression editor', () => {
    target = document.createElement('div');
    document.body.append(target);
    const node = { data: { kind: 'call', title: 'Query', nameSource: 'derived', fields: { inputs: structuredClone(literal) } } };
    component = mount(WorkflowNode, {
      target, props: { id: 'call', data: { node, width: 300, height: 200 } },
      context: new Map([['run', { overlay: {} }], ['mode', {}], ['ops', { entries: [] }]]),
    });
    flushSync();
    expect(target.querySelector('.wf-ro').textContent).toBe(editableText(literal));
    expect(target.querySelector('.expr-field')).toBeNull();
    expect(node.data.fields.inputs).toEqual(literal);
  });

  it('writes scalar edits with their tag', () => {
    target = document.createElement('div');
    document.body.append(target);
    const node = { data: { fields: { count: { kind: 'number', value: 2 } } } };
    component = mount(ArgField, { target, props: { node, fieldKey: 'count' } });
    flushSync();
    target.querySelector('button').click();
    flushSync();
    const input = target.querySelector('input');
    input.value = '7';
    input.dispatchEvent(new Event('input', { bubbles: true }));
    flushSync();
    expect(node.data.fields.count).toEqual({ kind: 'number', value: 7 });
  });
});
