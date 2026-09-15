import { describe, it, expect } from 'vitest';
import {
  catalogFieldsMap,
  fieldDefaultValue,
  operationSwitchPatch,
  operationsForKind,
  currentOperationId,
} from './operations.js';

const CATALOG = [
  {
    id: 'call.display',
    label: 'Display',
    nodeKind: 'call',
    operation: 'display',
    fields: [{ name: 'message', type: 'string', default: 'hi' }],
  },
  {
    id: 'call.record',
    label: 'Record',
    nodeKind: 'call',
    operation: 'record',
    receiver: 'ledger',
    fields: [
      { name: 'count', type: 'number', default: 3 },
      { name: 'value', type: 'expression', default: 'x + 1' },
    ],
  },
  {
    id: 'effect.sleep',
    label: 'Sleep',
    nodeKind: 'effect',
    effect: 'sleep',
    fields: [{ name: 'duration', type: 'number', default: 5 }],
  },
];

describe('fieldDefaultValue', () => {
  it('coerces each field type to an EditableValue', () => {
    expect(fieldDefaultValue({ type: 'number', default: 3 })).toBe(3);
    expect(fieldDefaultValue({ type: 'number' })).toBe(0);
    expect(fieldDefaultValue({ type: 'boolean', default: true })).toBe(true);
    expect(fieldDefaultValue({ type: 'expression', default: 'a + 1' })).toEqual({ $expr: 'a + 1' });
    expect(fieldDefaultValue({ type: 'string', default: 'hi' })).toBe('hi');
  });
});

describe('catalogFieldsMap', () => {
  it('builds a seed fields map from an operation entry', () => {
    expect(catalogFieldsMap(CATALOG[1])).toEqual({ count: 3, value: { $expr: 'x + 1' } });
  });
});

describe('operationSwitchPatch', () => {
  it('swaps a call receiver and refills its fields', () => {
    const patch = operationSwitchPatch('call', CATALOG[1]);
    expect(patch.operation).toBe('record');
    expect(patch.effect).toBeUndefined();
    expect(patch.clearExpression).toBeUndefined();
    expect(patch.fields).toEqual({ count: 3, value: { $expr: 'x + 1' } });
  });

  // FIG-3179: the chosen operation may belong to another receiver entirely, so
  // a switch carries the receiver and a re-synthesized call. Renaming only the
  // method left `display.record`, which no receiver serves.
  it('carries the new receiver and re-synthesizes the call expression', () => {
    const patch = operationSwitchPatch('call', CATALOG[1]);
    expect(patch.receiver).toBe('ledger');
    expect(patch.expression).toBe('await ledger.record({ count: 3, value: x + 1 })');
  });

  // A receiverless entry means the display catalog, and the patch says so
  // explicitly so a node switched away from a named receiver does not keep it.
  it('clears the receiver for an entry that names none', () => {
    const patch = operationSwitchPatch('call', CATALOG[0]);
    expect(patch.receiver).toBeNull();
    expect(patch.expression).toBe('await display.display({ message: "hi" })');
  });

  it('rebuilds an effect and clears any seeded expression', () => {
    const patch = operationSwitchPatch('effect', CATALOG[2]);
    expect(patch.effect).toBe('sleep');
    expect(patch.clearExpression).toBe(true);
    expect(patch.operation).toBeUndefined();
    expect(patch.fields).toEqual({ duration: 5 });
  });
});

describe('operationsForKind / currentOperationId', () => {
  it('lists operations for a node kind', () => {
    expect(operationsForKind(CATALOG, 'call').map((o) => o.id)).toEqual([
      'call.display',
      'call.record',
    ]);
    expect(operationsForKind(CATALOG, 'effect').map((o) => o.id)).toEqual(['effect.sleep']);
  });

  it('matches the entry a node currently uses', () => {
    const callNode = { data: { kind: 'call', operation: 'record' } };
    expect(currentOperationId(CATALOG, callNode)).toBe('call.record');
    const effectNode = { data: { kind: 'effect', effect: 'sleep' } };
    expect(currentOperationId(CATALOG, effectNode)).toBe('effect.sleep');
    const unknown = { data: { kind: 'call', operation: 'nope' } };
    expect(currentOperationId(CATALOG, unknown)).toBeNull();
  });
});
