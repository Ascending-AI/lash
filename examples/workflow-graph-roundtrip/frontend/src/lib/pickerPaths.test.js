import { it, expect } from 'vitest';
import { readFileSync } from 'node:fs';
import { parse } from 'svelte/compiler';
import { compatibleVarNames, expectedArgFieldType, expectedSlotType } from './facets.js';

function walk(value, visit) {
  if (!value || typeof value !== 'object') return;
  visit(value);
  for (const child of Object.values(value)) {
    if (Array.isArray(child)) child.forEach((entry) => walk(entry, visit));
    else if (child && typeof child === 'object') walk(child, visit);
  }
}

it('editor_expected_slots_reach_every_picker_path', () => {
  const availableVars = [{ name: 'text', type: 'str' }, { name: 'count', type: 'float' }, { name: 'rows', type: 'list[str]' }];
  const node = { data: { expectedArgTypes: [
    { slot: 'arg[0]', type: 'str' },
    { slot: 'arg[0]["query"]', type: 'str' },
    { slot: 'arg[0]["a.b"]', type: 'float' },
    { slot: 'arg[0]["items"]', type: 'list[str]' },
    { slot: 'arg[0]["items"][0]', type: 'str' },
    { slot: 'call[1].arg[0]["text"]', type: 'str' },
  ] } };
  // Positional inputs, named and nested fields, and list items use
  // the same variable filter at the public picker seam.
  for (const [slot, expected] of [
    ['arg[0]', ['text']], ['arg[0]["query"]', ['text']],
    ['arg[0]["a.b"]', ['count']], ['arg[0]["items"]', ['rows']],
    ['arg[0]["items"][0]', ['text']], ['call[1].arg[0]["text"]', ['text']],
  ]) expect(compatibleVarNames(availableVars, expectedSlotType(node, slot))).toEqual(expected);
  for (const [field, expected] of [['query', ['text']], ['a.b', ['count']], ['items', ['rows']], ['text', ['text']]]) {
    expect(compatibleVarNames(availableVars, expectedArgFieldType(node, field))).toEqual(expected);
  }
  expect(expectedArgFieldType(node, 'b')).toBeNull();

  // This bounds the public wiring pin to the parsed component props. It does
  // not claim that a helper test proves arbitrary browser interactions.
  for (const [file, component, typedCount] of [
    ['nodes/WorkflowNode.svelte', 'ExpressionField', 1],
    ['steps/ArgField.svelte', 'ExpressionField', 1],
    ['steps/EditableValue.svelte', 'ExpressionField', 1],
    ['nodes/ContainerNode.svelte', 'ExpressionField', 2],
    ['steps/StepCard.svelte', 'EditableValue', 2],
  ]) {
    const ast = parse(readFileSync(new URL(`../components/${file}`, import.meta.url), 'utf8'), { modern: true });
    const fields = [];
    walk(ast.fragment, (entry) => { if (entry.type === 'Component' && entry.name === component) fields.push(entry); });
    expect(fields.length, file).toBeGreaterThan(0);
    expect(fields.filter((field) => field.attributes.some((attribute) => attribute.name === 'expectedType')).length, file).toBe(typedCount);
  }
  const source = readFileSync(new URL('../components/ExpressionField.svelte', import.meta.url), 'utf8');
  const ast = parse(source, { modern: true });
  const filters = [];
  walk(ast.instance, (entry) => {
    if (entry.type === 'CallExpression' && entry.callee.name === 'compatibleVarNames') filters.push(entry.arguments.map((argument) => argument.name));
  });
  expect(filters).toEqual([['availableVars', 'expectedType']]);
});
