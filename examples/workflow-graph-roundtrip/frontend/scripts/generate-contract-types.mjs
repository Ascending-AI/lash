import { readFile, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

import { compileFromFile } from 'json-schema-to-typescript';

const frontend = path.resolve(path.dirname(fileURLToPath(import.meta.url)), '..');
const repository = path.resolve(frontend, '../../..');
const generated = path.join(frontend, 'src/generated');
const check = process.argv.slice(2).includes('--check');

const documents = [
  ['workflow-graph/v13.schema.json', 'workflow-graph.d.ts'],
  ['workflow-type-facets/v3.schema.json', 'workflow-type-facets.d.ts'],
];

const stale = [];
await mkdir(generated, { recursive: true });
for (const [schemaName, outputName] of documents) {
  const schema = path.join(repository, 'schemas/host', schemaName);
  const output = path.join(generated, outputName);
  const contents = await compileFromFile(schema, {
    bannerComment:
      '/* Generated from schemas/host by npm run generate:types. Do not edit directly. */',
    style: { singleQuote: true },
  });
  if (check) {
    const current = await readFile(output, 'utf8').catch(() => null);
    if (current !== contents) stale.push(path.relative(frontend, output));
  } else {
    await writeFile(output, contents);
  }
}

if (stale.length > 0) {
  console.error(`Generated contract types are stale:\n${stale.join('\n')}`);
  console.error('Run `npm run generate:types`.');
  process.exitCode = 1;
}
