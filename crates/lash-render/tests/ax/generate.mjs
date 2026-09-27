// Generate the pinned ASCII fixture from ax's smartStringify.
// Usage: node --experimental-strip-types generate.mjs /path/to/ax
// The final cut is ax runtime.ts's truncateText, copied here because runtime.ts
// imports the full agent runtime and cannot be loaded as a standalone module.
import { execFileSync } from 'node:child_process';
import { writeFileSync } from 'node:fs';
import { resolve } from 'node:path';
import { pathToFileURL } from 'node:url';

const ax = resolve(process.argv[2]);
const revision = execFileSync('git', ['-C', ax, 'rev-parse', 'HEAD'], { encoding: 'utf8' }).trim();
if (revision !== 'b780a14a3cb94d5ac572db04038399aef655c76c') {
  throw new Error(`expected pinned ax revision, got ${revision}`);
}
const { smartStringify } = await import(pathToFileURL(`${ax}/src/ax/agent/truncate.ts`).href);

function truncateText(text, maxChars) {
  if (text.length <= maxChars) return text;
  return `${text.slice(0, maxChars)}\n...[truncated ${text.length - maxChars} chars]`;
}

const cases = [
  ['null', null, 100],
  ['boolean', true, 100],
  ['number', 42, 100],
  ['object', { a: 1, b: { c: 'text' } }, 100],
  ['depth', { a: { b: { c: { d: 1 } } } }, 200],
  ['array10', Array.from({ length: 10 }, (_, i) => i), 200],
  ['array11', Array.from({ length: 11 }, (_, i) => i), 200],
  ['item', Array.from({ length: 11 }, (_, i) => i === 0 ? 'x'.repeat(100) : i), 200],
  ['stack', { stack: 'Error: boom\n    at a\n    at b\n    at c\n    at d\n    at e\n    at f' }, 200],
  ['final_cut', { a: 'abcdef', b: 'ghijkl' }, 12],
];

const fixture = cases.map(([name, input, max_chars]) => ({
  name,
  input,
  max_chars,
  body: truncateText(smartStringify(input, max_chars), max_chars),
}));
writeFileSync(new URL('./fixtures.json', import.meta.url), JSON.stringify(fixture, null, 2) + '\n');
