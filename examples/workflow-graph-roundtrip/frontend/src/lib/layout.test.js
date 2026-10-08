import { describe, expect, it } from 'vitest';
import { layoutDocument } from './layout.js';

describe('layoutDocument', () => {
  it('reserves the terminal expression row before a container add control', () => {
    const processId = 'process:blank';
    const terminalId = 'terminal:finish';
    const doc = {
      nodes: [
        {
          id: processId,
          type: 'process',
          data: {
            kind: 'process',
            params: [],
            children: [{ slot: 'body', nodeIds: [terminalId] }],
          },
        },
        {
          id: terminalId,
          type: 'terminal',
          parentId: processId,
          data: { kind: 'terminal', terminalKind: 'finish', expression: '0' },
        },
      ],
      edges: [],
      roots: { processes: [processId], main: [] },
    };

    const { positions, sizes, groupLayouts } = layoutDocument(doc);
    const terminalTop = positions.get(terminalId).y;
    const terminalBottom = terminalTop + sizes.get(terminalId).h;
    const body = groupLayouts.get(processId).find((group) => group.slot === 'body');
    const addControlTop = body.y + body.h - 32;

    expect(sizes.get(terminalId).h).toBeGreaterThanOrEqual(150);
    expect(addControlTop).toBeGreaterThan(terminalBottom);
  });
});
