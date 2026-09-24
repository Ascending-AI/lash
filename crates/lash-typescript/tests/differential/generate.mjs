import {
  mkdirSync,
  mkdtempSync,
  readFileSync,
  readdirSync,
  rmSync,
  unlinkSync,
  writeFileSync,
} from 'node:fs';
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
const findingsDirectory = join(directory, 'findings');
const expectationsDirectory = join(directory, 'expectations');

// Every `findings/<shard>.txt`, sorted by shard name. A shard is one file per
// ticket or review lane; a row's corpus id is `differential:<shard>:<n>` with
// n 1-based within the shard, so a new shard conflicts with nothing.
const shards = readdirSync(findingsDirectory)
  .filter((name) => name.endsWith('.txt'))
  .map((name) => name.slice(0, -'.txt'.length))
  .sort();

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

function expressions(shard) {
  return readFileSync(join(findingsDirectory, `${shard}.txt`), 'utf8')
    .split('\n')
    .filter((line) => line.length > 0);
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

mkdirSync(expectationsDirectory, { recursive: true });
const header = ['lane', 'index', 'disposition', 'expression', `node_${NODE_VERSION}`, 'diagnostic'];
let total = 0;
const distinct = new Set();
for (const shard of shards) {
  const rows = [header];
  for (const [offset, expression] of expressions(shard).entries()) {
    const { disposition, diagnostic } = dispositions.get(expression) ?? {
      disposition: 'accept',
      diagnostic: '-',
    };
    rows.push([
      shard,
      String(offset + 1),
      disposition,
      JSON.stringify(expression),
      JSON.stringify(nodeString(expression)),
      diagnostic,
    ]);
    distinct.add(JSON.stringify(expression));
  }
  writeFileSync(
    join(expectationsDirectory, `${shard}.tsv`),
    `${rows.map((row) => row.join('\t')).join('\n')}\n`,
  );
  total += rows.length - 1;
  console.log(`expectations/${shard}.tsv: ${rows.length - 1} rows`);
}

// A shard's table is stale once its findings file is gone.
for (const name of readdirSync(expectationsDirectory)) {
  if (name.endsWith('.tsv') && !shards.includes(name.slice(0, -'.tsv'.length))) {
    unlinkSync(join(expectationsDirectory, name));
    console.log(`expectations/${name}: removed (no findings/${name.slice(0, -'.tsv'.length)}.txt)`);
  }
}
console.log(`total: ${total} rows, ${distinct.size} distinct expressions`);
