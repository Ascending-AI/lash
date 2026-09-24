import { mkdtempSync, readFileSync, rmSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { tmpdir } from 'node:os';
import { spawnSync } from 'node:child_process';
import { fileURLToPath } from 'node:url';

const NODE_VERSION = 'v25.2.1';
const TYPESCRIPT_VERSION = '7.0.2';
if (process.version !== NODE_VERSION) {
  throw new Error(`oracle requires Node ${NODE_VERSION}, got ${process.version}`);
}

const directory = dirname(fileURLToPath(import.meta.url));
const lanes = [
  ['opus', 'opus-expressions.txt', 163],
  ['sol', 'sol-expressions.txt', 124],
  ['findings', 'findings-expressions.txt', 301],
];

// Every non-accepted expression and the diagnostic it must name, from
// `dispositions.tsv`, which the oracle test also reads.
const dispositions = new Map(
  readFileSync(join(directory, 'dispositions.tsv'), 'utf8')
    .split('\n')
    .filter((line) => line.length > 0 && !line.startsWith('#'))
    .map((line) => {
      const [expression, disposition, diagnostic] = line.split('\t');
      if (!['reject', 'runtime-reject', 'open-defect', 'accept-unlinked'].includes(disposition) || !diagnostic) {
        throw new Error(`malformed disposition row: ${line}`);
      }
      return [JSON.parse(expression), { disposition, diagnostic }];
    }),
);

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
    if (/\b(?:const\s+)?enum\b/u.test(expression)) {
      return typescriptNodeString(expression);
    }
    return String(eval(`(${eraseTypesForNode(expression)})`));
  } catch (error) {
    return `ERR<${error.constructor.name}>`;
  }
}

function typescriptNodeString(expression) {
  const work = mkdtempSync(join(tmpdir(), 'lash-typescript-oracle-'));
  try {
    const input = join(work, 'oracle.ts');
    writeFileSync(input, `const __result = String(${expression});\n__result;\n`);
    const compile = spawnSync(
      'npx',
      [
        '--yes',
        '--package',
        `typescript@${TYPESCRIPT_VERSION}`,
        'tsc',
        '--target',
        'esnext',
        '--outDir',
        work,
        input,
        '--pretty',
        'false',
      ],
      { encoding: 'utf8' },
    );
    if (compile.status !== 0) {
      throw new Error(
        `TypeScript ${TYPESCRIPT_VERSION} oracle compile failed:\n${compile.stdout}${compile.stderr}`,
      );
    }
    return String(eval(readFileSync(join(work, 'oracle.js'), 'utf8')));
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

const rows = [['lane', 'index', 'disposition', 'expression', `node_${NODE_VERSION}`, 'diagnostic']];
for (const [lane, file, expectedCount] of lanes) {
  for (const [offset, expression] of expressions(file, expectedCount).entries()) {
    const { disposition, diagnostic } = dispositions.get(expression) ?? {
      disposition: 'accept',
      diagnostic: '-',
    };
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
