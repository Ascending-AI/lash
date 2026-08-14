import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';

const NODE_VERSION = 'v25.2.1';
if (process.version !== NODE_VERSION) {
  throw new Error(`oracle requires Node ${NODE_VERSION}, got ${process.version}`);
}

const directory = dirname(fileURLToPath(import.meta.url));
const lanes = [
  ['opus', 'opus-expressions.txt', 163],
  ['sol', 'sol-expressions.txt', 124],
  ['findings', 'findings-expressions.txt', 23],
];

const rejected = new Map([
  ['2 ** 3', 'TS_EXPONENTIATION_UNSUPPORTED'],
  ['1 & 3', 'TS_BITWISE_UNSUPPORTED'],
  ['1 | 2', 'TS_BITWISE_UNSUPPORTED'],
  ['1 ^ 3', 'TS_BITWISE_UNSUPPORTED'],
  ['~1', 'TS_BITWISE_UNSUPPORTED'],
  ['1 << 2', 'TS_BITWISE_UNSUPPORTED'],
  ['8 >> 2', 'TS_BITWISE_UNSUPPORTED'],
  ['-8 >>> 2', 'TS_BITWISE_UNSUPPORTED'],
  ['(1, 2)', 'TS_SEQUENCE_UNSUPPORTED'],
  ["'a' in ({a:1})", 'TS_IN_OPERATOR_UNSUPPORTED'],
  ['1 instanceof Object', 'TS_INSTANCEOF_UNSUPPORTED'],
  ['delete ({a:1}).a', 'TS_DELETE_UNSUPPORTED'],
  ['null ?? 1 || 2', 'TS_SYNTAX_ERROR'],
  ["'\\uD800'", 'TS_LONE_SURROGATE_LITERAL_UNSUPPORTED'],
  ["'abc'.split('')", 'TS_METHOD_UNSUPPORTED'],
  ["'a,b'.split(',')", 'TS_METHOD_UNSUPPORTED'],
  ["''.split(',')", 'TS_METHOD_UNSUPPORTED'],
  ["'abc'.split('b')", 'TS_METHOD_UNSUPPORTED'],
  ["'\\uD83D\\uDE00'.split('')", 'TS_METHOD_UNSUPPORTED'],
]);

const runtimeRejected = new Map([
  [
    '(() => { const a = [1]; a[3] = 9; return `${a.length}|${a[1]}|${a[2]}|${a[3]}`; })()',
    'TS_SPARSE_ARRAY_UNSUPPORTED',
  ],
  [
    '(() => { const a = [1,2]; a[-1] = 9; return a[-1]; })()',
    'TS_ARRAY_NON_INDEX_PROPERTY_UNSUPPORTED',
  ],
  ["'\\uD83D\\uDE00'[0]", 'TS_LONE_SURROGATE_UNSUPPORTED'],
]);

function expressions(file, expectedCount) {
  const values = readFileSync(join(directory, file), 'utf8')
    .split('\n')
    .filter((line) => line.length > 0);
  if (values.length !== expectedCount) {
    throw new Error(`${file}: expected ${expectedCount} rows, got ${values.length}`);
  }
  return values;
}

function eraseTypesForNode(expression) {
  return expression
    .replace(/\bthis\s*:\s*number\s*,/gu, '')
    .replace(/:\s*number\b/gu, '');
}

function nodeString(expression) {
  try {
    return String(eval(`(${eraseTypesForNode(expression)})`));
  } catch (error) {
    return `ERR<${error.constructor.name}>`;
  }
}

const rows = [['lane', 'index', 'disposition', 'expression', `node_${NODE_VERSION}`, 'diagnostic']];
for (const [lane, file, expectedCount] of lanes) {
  for (const [offset, expression] of expressions(file, expectedCount).entries()) {
    const diagnostic = rejected.get(expression) ?? runtimeRejected.get(expression) ?? '-';
    const disposition = rejected.has(expression)
      ? 'reject'
      : runtimeRejected.has(expression)
        ? 'runtime-reject'
        : 'accept';
    rows.push([
      lane,
      String(offset + 1),
      disposition,
      JSON.stringify(expression),
      JSON.stringify(nodeString(expression)),
      diagnostic,
    ]);
  }
}

writeFileSync(
  join(directory, 'expectations.tsv'),
  `${rows.map((row) => row.join('\t')).join('\n')}\n`,
);
