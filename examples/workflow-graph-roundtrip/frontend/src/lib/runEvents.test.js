import { describe, expect, it } from 'vitest';
import { acceptsRunEvent } from './runEvents.js';

describe('acceptsRunEvent', () => {
  const event = (definition) => ({
    runId: 'run',
    workflowVersion: 2,
    definition,
    sequence: 1,
    nodeId: 'node:a',
    status: 'started',
  });

  it('accepts an event from the loaded definition', () => {
    expect(acceptsRunEvent('def-1', event('def-1'))).toBe(true);
  });

  it('refuses an event from another definition that shares node ids', () => {
    expect(acceptsRunEvent('def-1', event('def-2'))).toBe(false);
  });

  it('refuses every event when the loaded graph claims no definition', () => {
    expect(acceptsRunEvent(null, event('def-1'))).toBe(false);
    expect(acceptsRunEvent(undefined, event(undefined))).toBe(false);
  });
});
