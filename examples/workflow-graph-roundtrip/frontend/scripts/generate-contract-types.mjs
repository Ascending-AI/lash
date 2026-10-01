import { readFile, readdir, mkdir, writeFile } from 'node:fs/promises';
import { fileURLToPath } from 'node:url';
import path from 'node:path';

import { compile } from 'json-schema-to-typescript';
import { schemaForTypescript } from './schema-for-typescript.mjs';

const frontend = path.resolve(
  path.dirname(fileURLToPath(import.meta.url)),
  '..',
);
const repository = path.resolve(frontend, '../../..');
const generated = path.join(frontend, 'src/generated');
const check = process.argv.slice(2).includes('--check');

async function currentSchema(shape) {
  const directory = path.join(repository, 'schemas/host', shape);
  const files = (await readdir(directory)).filter((name) => /^v\d+\.schema\.json$/.test(name));
  if (files.length !== 1) {
    throw new Error(`${shape} must publish exactly one current schema, found ${files.length}`);
  }
  return `${shape}/${files[0]}`;
}

const documents = [
  [await currentSchema('workflow-graph'), 'workflow-graph.d.ts'],
  [await currentSchema('workflow-type-facets'), 'workflow-type-facets.d.ts'],
  [
    '../../examples/workflow-graph-roundtrip/frontend/src/generated/workflow-document.schema.json',
    'workflow-document.d.ts',
  ],
  [
    '../../examples/workflow-graph-roundtrip/frontend/src/generated/error-response.schema.json',
    'error-response.d.ts',
  ],
];

const stale = [];
await mkdir(generated, { recursive: true });
for (const [schemaName, outputName] of documents) {
  const schema = schemaName.startsWith('../')
    ? path.resolve(repository, 'schemas/host', schemaName)
    : path.join(repository, 'schemas/host', schemaName);
  const output = path.join(generated, outputName);
  const contents = await compile(
    schemaForTypescript(JSON.parse(await readFile(schema, 'utf8'))),
    path.basename(schema, path.extname(schema)),
    {
      cwd: path.dirname(schema),
      bannerComment: schemaName.startsWith('../')
        ? '/* Generated from the example Rust HTTP DTOs by npm run generate:types. Do not edit directly. */'
        : '/* Generated from schemas/host by npm run generate:types. Do not edit directly. */',
      style: { singleQuote: true },
    },
  );
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
