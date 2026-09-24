// The Node session oracle's generator (FIG-3599).
//
// Reads `corpus.txt` and writes `expectations.json`: every session's cells
// with the reference answer of the pinned Node, which runs each cell as a
// successive classic Script in ONE realm (`vm.createContext` plus
// `vm.Script`). Top-level `let`/`const`/`class` therefore live in the realm's
// shared global lexical environment and `var`/function declarations on its
// global object, exactly as ECMA-262 GlobalDeclarationInstantiation specifies.
//
// The mapping from a lash cell to that reference (the README next to this
// file states it in full, and ADR 0062 records it):
//
// * A cell is a Script. Its observation is its printed lines, how it ended
//   (normally, through `finish(value)`, or by throwing an error of a class),
//   and one binding-visibility probe per session binder name, run as its own
//   Script after the cell.
// * `console.log`/`warn`/`error`/`info`/`debug` are the host-defined printer
//   of deviation register entry 13: arguments joined by one space, a plain
//   object or array as compact JSON, every other value as ECMA `ToString`.
// * `finish(value)` records the value and ends the cell; nothing after it in
//   the cell runs.
// * A Script's completion value is not observed: a lash cell surfaces none.
// * A cell the dialect statically rejects (`cell reject TS_*`) never enters
//   the realm, as it never runs in lash.
// * Top-level `await` has no classic-Script meaning (ECMA-262 permits it only
//   in a Module, whose declarations are not global), so the corpus holds no
//   await cell; one would fail here as the SyntaxError it is.
// * A probe is `console.log(typeof NAME, JSON.stringify(NAME))`. It prints the
//   binding's type and JSON; a ReferenceError answers `unbound`, or `tdz` when
//   the binding exists but is uninitialized. The generator also records the
//   names whose value reaches a function, which the registered
//   `closure-boundary` deviation turns into `unbound` on the lash side.
//
// Regeneration is deliberate and byte-identical, as for `../generate.mjs`:
//
//   node crates/lash-typescript/tests/differential/sessions/generate.mjs
//
// The generator refuses any Node other than the stamped version.

import { readFileSync, writeFileSync } from 'node:fs';
import { dirname, join } from 'node:path';
import { fileURLToPath } from 'node:url';
import vm from 'node:vm';

const NODE_VERSION = 'v25.2.1';
if (process.version !== NODE_VERSION) {
  throw new Error(`oracle requires Node ${NODE_VERSION}, got ${process.version}`);
}

const directory = dirname(fileURLToPath(import.meta.url));

// --- corpus.txt --------------------------------------------------------------

function parseCorpus(text) {
  const sessions = [];
  let session = null;
  let cell = null;
  const lines = text.split('\n');
  if (lines.at(-1) === '') lines.pop();
  const closeCell = () => {
    if (cell) {
      if ((cell.deviation || cell.defect) && !cell.lash) {
        throw new Error(`deviation or defect cell in ${session.id} states no lash answer`);
      }
      delete cell.answered;
      while (cell.lines.length && cell.lines.at(-1) === '') cell.lines.pop();
      cell.source = cell.lines.join('\n') + '\n';
      delete cell.lines;
      session.cells.push(cell);
      cell = null;
    }
  };
  for (const [index, line] of lines.entries()) {
    const where = `corpus.txt:${index + 1}`;
    if (cell && !cell.answered && !/^(cell\b|lash(-resident|-restart)? |end$)/.test(line)) {
      cell.lines.push(line);
      continue;
    }
    if (line === '' || line.startsWith('#')) continue;
    const [keyword, ...rest] = line.split(' ');
    const argument = rest.join(' ');
    if (keyword === 'session') {
      if (session) throw new Error(`${where}: session ${session.id} has no end`);
      session = { id: argument, about: '', probe: [], deviations: [], cells: [] };
      continue;
    }
    if (!session) throw new Error(`${where}: \`${keyword}\` outside a session`);
    if (keyword === 'about') {
      session.about = argument;
    } else if (keyword === 'probe') {
      session.probe = rest;
    } else if (keyword === 'deviation' && !cell) {
      session.deviations.push(argument);
    } else if (keyword === 'cell') {
      closeCell();
      if (session.cells.at(-1)?.lash) {
        throw new Error(`${where}: a deviation or defect cell ends its session`);
      }
      const [kind, value] = rest;
      cell = { reject: null, deviation: null, defect: null, lines: [] };
      if (kind === 'reject') cell.reject = value;
      else if (kind === 'deviation') cell.deviation = value;
      else if (kind === 'defect') cell.defect = value;
      else if (kind !== undefined) throw new Error(`${where}: unknown cell kind ${kind}`);
    } else if (keyword === 'lash' || keyword === 'lash-resident' || keyword === 'lash-restart') {
      if (!cell?.deviation && !cell?.defect) {
        throw new Error(`${where}: only a deviation or defect cell states a lash answer`);
      }
      cell.answered = true;
      cell.lash ??= {};
      const answer = JSON.parse(argument);
      if (keyword !== 'lash-restart') cell.lash.resident = answer;
      if (keyword !== 'lash-resident') cell.lash.restart = answer;
    } else if (keyword === 'end') {
      closeCell();
      sessions.push(session);
      session = null;
    } else {
      throw new Error(`${where}: unknown line \`${line}\``);
    }
  }
  if (session) throw new Error(`session ${session.id} has no end`);
  return sessions;
}

// --- the realm ---------------------------------------------------------------

// Installs the lash host surface into a fresh realm. It declares nothing: the
// host functions are non-enumerable global-object properties, which is where a
// host's own globals live.
const SETUP = `(() => {
  const host = globalThis.__lashOracleHost;
  delete globalThis.__lashOracleHost;
  const plain = (value) => {
    if (Array.isArray(value)) return true;
    if (value === null || typeof value !== 'object') return false;
    const prototype = Object.getPrototypeOf(value);
    return prototype === Object.prototype || prototype === null;
  };
  const render = (value) => (plain(value) ? JSON.stringify(value) : String(value));
  const log = (...values) => host.print(values.map(render).join(' '));
  const define = (name, value) =>
    Object.defineProperty(globalThis, name, { value, writable: true, configurable: true, enumerable: false });
  define('console', { log, warn: log, error: log, info: log, debug: log });
  const finished = { finished: true };
  host.sentinel(finished);
  define('finish', (value) => {
    host.finish(JSON.stringify(value === undefined ? null : value));
    throw finished;
  });
  const reach = (value, seen) => {
    if (typeof value === 'function') return true;
    if (value === null || typeof value !== 'object' || seen.has(value)) return false;
    seen.add(value);
    if (value instanceof Map) return [...value].some(([key, item]) => reach(key, seen) || reach(item, seen));
    if (value instanceof Set) return [...value].some((item) => reach(item, seen));
    return Object.values(value).some((item) => reach(item, seen));
  };
  host.reach((value) => reach(value, new Set()));
})();`;

function realm() {
  const state = { prints: [], finish: undefined, sentinel: undefined, reach: undefined };
  const context = vm.createContext({
    __lashOracleHost: {
      print: (text) => state.prints.push(text),
      finish: (json) => {
        state.finish = json;
      },
      sentinel: (value) => {
        state.sentinel = value;
      },
      reach: (reach) => {
        state.reach = reach;
      },
    },
  });
  new vm.Script(SETUP).runInContext(context);
  return { context, state };
}

function errorClass(error) {
  if (error !== null && (typeof error === 'object' || typeof error === 'function')) {
    return typeof error.name === 'string' ? error.name : 'Object';
  }
  return typeof error;
}

function runCell({ context, state }, source) {
  state.prints = [];
  state.finish = undefined;
  const observation = { outcome: 'normal', prints: [] };
  try {
    new vm.Script(source).runInContext(context);
  } catch (error) {
    if (state.finish !== undefined && error !== state.sentinel) {
      throw new Error(`a cell caught finish's end of the cell, which the mapping excludes:\n${source}`);
    }
    if (error === state.sentinel) {
      observation.outcome = 'finish';
      observation.finish = JSON.parse(state.finish);
    } else {
      observation.outcome = 'throw';
      observation.error = errorClass(error);
    }
  }
  if (state.finish !== undefined && observation.outcome !== 'finish') {
    throw new Error(`a cell caught finish's end of the cell, which the mapping excludes:\n${source}`);
  }
  observation.prints = state.prints;
  return observation;
}

function probe({ context, state }, name) {
  state.prints = [];
  let answer;
  try {
    new vm.Script(`console.log(typeof ${name}, JSON.stringify(${name}));`).runInContext(context);
    answer = state.prints.join('\n');
  } catch (error) {
    if (errorClass(error) === 'ReferenceError') {
      answer = /before initialization/.test(error.message) ? 'tdz' : 'unbound';
    } else {
      answer = `throws ${errorClass(error)}`;
    }
  }
  let closure = false;
  try {
    closure = state.reach(new vm.Script(name).runInContext(context));
  } catch {
    closure = false;
  }
  return { answer, closure };
}

// --- generation --------------------------------------------------------------

const sessions = parseCorpus(readFileSync(join(directory, 'corpus.txt'), 'utf8'));
const output = { node: NODE_VERSION, sessions: [] };
for (const session of sessions) {
  const engine = realm();
  const cells = [];
  for (const cell of session.cells) {
    const node = cell.reject
      ? { outcome: 'rejected', prints: [] }
      : runCell(engine, cell.source);
    node.probes = {};
    node.closures = [];
    for (const name of session.probe) {
      const { answer, closure } = probe(engine, name);
      node.probes[name] = answer;
      if (closure) node.closures.push(name);
    }
    const entry = {
      source: cell.source,
      reject: cell.reject,
      deviation: cell.deviation,
      defect: cell.defect,
      node,
    };
    if (cell.lash) entry.lash = cell.lash;
    cells.push(entry);
  }
  output.sessions.push({
    id: session.id,
    about: session.about,
    probe: session.probe,
    deviations: session.deviations,
    cells,
  });
}
writeFileSync(join(directory, 'expectations.json'), `${JSON.stringify(output, null, 2)}\n`);
