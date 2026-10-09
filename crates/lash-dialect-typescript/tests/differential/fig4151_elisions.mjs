// Node v25.2.1. Run from the repository root to regenerate the witnesses.
import fs from 'node:fs';
import assert from 'node:assert/strict';
assert.equal(process.version, 'v25.2.1');
const probe = `(function () { const each = []; a.forEach((v, i) => { each.push([i, v]); }); const of = []; for (const v of a) { of.push(v === undefined ? 'undefined' : v); } return { length: a.length, inside: [0 in a, 1 in a, 2 in a], own: [a.hasOwnProperty('0'), a.hasOwnProperty('1'), a.hasOwnProperty('2')], each, of }; })()`;
const fixtures = ['[,2]', '[0,,2]', '[1,,]'].map(literal => ({literal, probe, expected: Function(`const a = ${literal}; return ${probe};`)()}));
fs.writeFileSync('crates/lash-typescript/tests/agent_surface/literal_elisions.json', JSON.stringify(fixtures, null, 2) + '\n');

const expressions = fixtures.map(({literal, probe}) => `(function () { const a = ${literal}; return JSON.stringify(${probe}); })()`);
fs.writeFileSync('crates/lash-typescript/tests/differential/findings/FIG-4151.txt', expressions.join('\n') + '\n');
const header = 'lane\tindex\tdisposition\texpression\tnode_v25.2.1\tdiagnostic';
const rows = expressions.map((expression, index) => ['FIG-4151', index + 1, 'accept', JSON.stringify(expression), JSON.stringify(JSON.stringify(fixtures[index].expected)), '-'].join('\t'));
fs.writeFileSync('crates/lash-typescript/tests/differential/expectations/FIG-4151.tsv', [header, ...rows].join('\n') + '\n');
