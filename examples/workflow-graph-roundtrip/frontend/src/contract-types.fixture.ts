import type { EdgeData, NodeData } from './generated/workflow-document';

const name = { nameSource: 'derived', title: 'node' } as const;
const data: NodeData = { ...name, kind: 'data', expression: '1' };
const effect: NodeData = { ...name, kind: 'effect', effect: 'sleep_for' };
const sequence: EdgeData = { kind: 'sequence', scope: 'main' };
const dependency: EdgeData = { kind: 'data_dependency', scope: 'main', variable: 'x', version: 1 };

// @ts-expect-error A data node cannot carry a condition.
const foreign: NodeData = { ...name, kind: 'data', condition: 'true' };
// @ts-expect-error Effect names use the core enum vocabulary.
const misspelled: NodeData = { ...name, kind: 'effect', effect: 'sleep' };
// @ts-expect-error A sequence cannot carry dependency data.
const crossed: EdgeData = { kind: 'sequence', scope: 'main', variable: 'x', version: 1 };
// @ts-expect-error A dependency must identify its variable version.
const incomplete: EdgeData = { kind: 'data_dependency', scope: 'main', variable: 'x' };

void [data, effect, sequence, dependency, foreign, misspelled, crossed, incomplete];
