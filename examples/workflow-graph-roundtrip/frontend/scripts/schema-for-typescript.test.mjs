import assert from 'node:assert/strict';
import { test } from 'node:test';
import { compile } from 'json-schema-to-typescript';
import { schemaForTypescript } from './schema-for-typescript.mjs';

test('2020-12 tuple members survive references, nulls and array nesting', async () => {
  const schema = {
    $defs: {
      Pair: {
        type: 'array',
        prefixItems: [{ type: 'string' }, { type: ['integer', 'null'] }],
        minItems: 2,
        maxItems: 2,
      },
    },
    type: 'object',
    properties: { pairs: { type: 'array', items: { $ref: '#/$defs/Pair' } } },
    required: ['pairs'],
    additionalProperties: false,
  };
  const original = structuredClone(schema);
  const types = await compile(schemaForTypescript(schema), 'Document', {
    bannerComment: '',
  });
  assert.match(types, /\[string, number \| null\]/);
  assert.match(types, /pairs: Pair\[\]/);
  assert.deepEqual(schema, original);
});

test('2020-12 tuple tail constraints survive TypeScript compilation', async () => {
  for (const [items, expected] of [
    [false, /\[string\]/],
    [{ type: 'boolean' }, /\[string, \.\.\.boolean\[\]\]/],
    [undefined, /\[string, \.\.\.unknown\[\]\]/],
  ]) {
    const types = await compile(
      schemaForTypescript({
        type: 'array',
        prefixItems: [{ type: 'string' }],
        minItems: 1,
        items,
      }),
      'Tuple',
      { bannerComment: '' },
    );
    assert.match(types, expected);
  }
});

test('reference siblings retain both referenced fields and discriminator fields', async () => {
  const types = await compile(
    schemaForTypescript({
      $defs: {
        FunctionDecl: {
          type: 'object',
          properties: {
            name: { type: 'string' },
            body: { type: 'string' },
          },
          required: ['name', 'body'],
        },
      },
      type: 'object',
      properties: {
        declaration: {
          $ref: '#/$defs/FunctionDecl',
          type: 'object',
          properties: { kind: { type: 'string', const: 'function' } },
          required: ['kind'],
        },
      },
      required: ['declaration'],
      additionalProperties: false,
    }),
    'Document',
    { bannerComment: '' },
  );
  assert.match(types, /name: string/);
  assert.match(types, /body: string/);
  assert.match(types, /kind: ["']function["']/);
});

test('reference annotations preserve scalar types without object intersections', async () => {
  const types = await compile(
    schemaForTypescript({
      $defs: { Scalar: { type: ['string', 'null'] } },
      type: 'object',
      properties: {
        value: { $ref: '#/$defs/Scalar', description: 'A nullable scalar.' },
      },
      required: ['value'],
      additionalProperties: false,
    }),
    'Document',
    { bannerComment: '' },
  );
  assert.match(types, /value: Scalar;/);
  assert.match(types, /export type Scalar = string \| null/);
});
