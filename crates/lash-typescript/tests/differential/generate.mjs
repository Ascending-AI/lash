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
import vm from 'node:vm';

import { NODE_VERSION, requirePinnedNode } from './sessions/realm.mjs';

const TYPESCRIPT_VERSION = '7.0.2';
// requirePinnedNode also pins TZ=UTC inside this process (FIG-3812): a Date
// row answers the same bytes whatever timezone the host runs in.
requirePinnedNode();

const directory = dirname(fileURLToPath(import.meta.url));
const findingsDirectory = join(directory, 'findings');
const expectationsDirectory = join(directory, 'expectations');

// --- the row realm -----------------------------------------------------------
//
// Every row evaluates in a fresh realm (`vm.createContext`), so no row can
// mutate another's intrinsics: `findings/FIG-3737.txt` deletes
// `RegExp.prototype.global` in one row, and before this isolation every
// g-flag row after it regenerated polluted (FIG-3812).
//
// A bare realm carries only ECMAScript intrinsics; Node's own extra globals
// (`URL`, `URLSearchParams`, `process`, `fetch`, ...) are the host's, as
// `sessions/realm.mjs` states for the session corpus's surface. Each row's
// realm gets them installed with their host descriptors, and each host
// class whose prototype tops out at the host's `Object.prototype` is
// re-rooted at the realm's, so `new URL('https://x/') instanceof Object`
// answers as the host realm's would. The intrinsics a row can mutate are
// fresh; the host surface is shared (a row mutating a host class itself,
// as opposed to an intrinsic, would still reach the next realm).

// The globals a bare realm already carries; computed once before any host
// class is re-rooted.
const BARE_REALM_GLOBALS = new Set(
  vm.runInContext('Object.getOwnPropertyNames(globalThis)', vm.createContext({})),
);

// The host globals a bare realm lacks, with each one's own descriptor, and
// whether it is a host class whose prototype tops out at the host's
// `Object.prototype` (re-rooted per realm — computed once, before the first
// re-rooting makes the question unanswerable).
const HOST_GLOBALS = Object.getOwnPropertyNames(globalThis)
  .filter((name) => !BARE_REALM_GLOBALS.has(name))
  .map((name) => {
    const descriptor = Object.getOwnPropertyDescriptor(globalThis, name);
    const value = descriptor.value;
    const reroot =
      typeof value === 'function' &&
      value.prototype !== undefined &&
      value.prototype !== null &&
      typeof value.prototype === 'object' &&
      Object.getPrototypeOf(value.prototype) === Object.prototype;
    return { name, descriptor, reroot };
  });

const SETUP = `(() => {
  const host = globalThis.__lashDifferentialHost;
  delete globalThis.__lashDifferentialHost;
  for (const { name, descriptor, reroot } of host) {
    if (reroot) Object.setPrototypeOf(descriptor.value.prototype, Object.prototype);
    Object.defineProperty(globalThis, name, descriptor);
  }
})();`;
const SETUP_SCRIPT = new vm.Script(SETUP);

function freshRealm() {
  const context = vm.createContext({ __lashDifferentialHost: HOST_GLOBALS });
  SETUP_SCRIPT.runInContext(context);
  return context;
}

// -----------------------------------------------------------------------------

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

// A row's script runs strict, as the module-scope `eval` it replaces did:
// `eval` in module code evaluates strict, and a `vm` Script is sloppy unless
// its prologue says otherwise. The checked-in answers are strict answers.
const STRICT = `'use strict';`;

function nodeString(expression) {
  const context = freshRealm();
  try {
    if (/\b(?:const\s+)?enum\b/u.test(expression)) {
      return typescriptNodeString(expression, context);
    }
    return String(vm.runInContext(`${STRICT}(${eraseTypesForNode(expression)})`, context));
  } catch (error) {
    return `ERR<${error.constructor.name}>`;
  }
}

function typescriptNodeString(expression, context) {
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
    return String(
      vm.runInContext(`${STRICT}${readFileSync(join(work, 'oracle.js'), 'utf8')}`, context),
    );
  } finally {
    rmSync(work, { recursive: true, force: true });
  }
}

// --- self-test ---------------------------------------------------------------
//
// The generator proves its own isolation before it writes anything
// (FIG-3812): a synthetic row that deletes an intrinsic must not reach the
// next row's realm, the pinned UTC must hold inside a realm, and the host
// surface must be installed. A failure here writes nothing.
{
  const deleted = nodeString(
    "(() => { delete RegExp.prototype.global; return typeof RegExp.prototype.global; })()",
  );
  const next = nodeString('typeof /x/g.global');
  if (deleted !== 'undefined' || next !== 'boolean') {
    throw new Error(
      `realm isolation self-test failed: deleting RegExp.prototype.global answered ` +
        `${JSON.stringify(deleted)} and the next row saw ${JSON.stringify(next)}, ` +
        `expected "undefined" then "boolean"`,
    );
  }
  const epoch = nodeString('new Date(0).toString()');
  if (epoch !== 'Thu Jan 01 1970 00:00:00 GMT+0000 (Coordinated Universal Time)') {
    throw new Error(
      `UTC self-test failed: new Date(0).toString() answered ${JSON.stringify(epoch)}`,
    );
  }
  const host = nodeString('typeof URL');
  if (host !== 'function') {
    throw new Error(`host surface self-test failed: typeof URL answered ${JSON.stringify(host)}`);
  }
  console.log('self-test: row realms are isolated, UTC is pinned, the host surface is installed');
}

// -----------------------------------------------------------------------------

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
