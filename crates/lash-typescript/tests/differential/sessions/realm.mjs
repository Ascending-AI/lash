// The Node session oracle's realm: the one statement, in code, of how a lash
// cell maps to a Script (FIG-3599). `generate.mjs` runs the hand-written
// corpus through it and `oracle.mjs` serves generated sessions through it
// (FIG-3608), so both corpora are answered by the same mapping.
//
// The mapping (the README next to this file states it in full, and ADR 0062
// records it):
//
// * A cell is a Script, run as a successive classic Script in ONE realm
//   (`vm.createContext` plus `vm.Script`). Top-level `let`/`const`/`class`
//   therefore live in the realm's shared global lexical environment and
//   `var`/function declarations on its global object, exactly as ECMA-262
//   GlobalDeclarationInstantiation specifies.
// * Its observation is its printed lines, how it ended (normally, through
//   `finish(value)`, or by throwing an error of a class), and one
//   binding-visibility probe per session binder name, run as its own Script
//   after the cell.
// * `console.log`/`warn`/`error`/`info`/`debug` are the host-defined printer
//   of deviation register entry 13: arguments joined by one space, a plain
//   object or array as compact JSON, every other value as ECMA `ToString`.
// * `URL` and `URLSearchParams` are the host's WHATWG classes, as they are
//   Node's own globals; a bare `vm` context lacks them.
// * `finish(value)` records the value and ends the cell; nothing after it in
//   the cell runs.
// * A Script's completion value is not observed: a lash cell surfaces none.
// * A cell the dialect statically rejects never enters the realm, as it never
//   runs in lash.
// * Top-level `await` has no classic-Script meaning (ECMA-262 permits it only
//   in a Module, whose declarations are not global), so no corpus holds an
//   await cell; one would fail here as the SyntaxError it is.
// * A probe is `console.log(typeof NAME, JSON.stringify(NAME))`. It prints the
//   binding's type and JSON; a ReferenceError answers `unbound`, or `tdz` when
//   the binding exists but is uninitialized. The realm also records the names
//   whose value reaches a function, which the registered `closure-boundary`
//   deviation turns into `unbound` on the lash side.

import vm from 'node:vm';

export const NODE_VERSION = 'v25.2.1';

/// Refuses any Node other than the stamped one: every answer is that Node's.
export function requirePinnedNode() {
  if (process.version !== NODE_VERSION) {
    throw new Error(`oracle requires Node ${NODE_VERSION}, got ${process.version}`);
  }
}

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
  // WHATWG URL is a host global in Node's main realm and in the dialect, and
  // absent from a bare \`vm\` context, so the host installs it here. Its
  // prototypes are re-parented onto this realm's \`Object.prototype\`, as they
  // are in a single realm, so \`url instanceof Object\` holds.
  for (const Class of [host.URL, host.URLSearchParams]) {
    Object.setPrototypeOf(Class.prototype, Object.prototype);
    define(Class.name, Class);
  }
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

/// A fresh realm with the lash host surface installed, and `host`'s bindings:
/// read-only globals the host supplies (a lazy projection on the lash side, a
/// plain value here).
export function realm(host = {}) {
  const state = { prints: [], finish: undefined, sentinel: undefined, reach: undefined };
  const context = vm.createContext({
    __lashOracleHost: {
      URL,
      URLSearchParams,
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
  for (const [name, value] of Object.entries(host)) {
    Object.defineProperty(context, name, {
      value: JSON.parse(JSON.stringify(value)),
      writable: false,
      configurable: false,
      enumerable: false,
    });
  }
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

/// Node's observation of every cell of one session, in one fresh realm.
///
/// `cells` are `{ source, reject }`: a cell the dialect rejects statically
/// (`reject` set) never enters the realm and is observed as `rejected`.
export function observeSession(probeNames, cells, host = {}) {
  const engine = realm(host);
  return cells.map((cell) => {
    const node = cell.reject ? { outcome: 'rejected', prints: [] } : runCell(engine, cell.source);
    node.probes = {};
    node.closures = [];
    for (const name of probeNames) {
      const { answer, closure } = probe(engine, name);
      node.probes[name] = answer;
      if (closure) node.closures.push(name);
    }
    return node;
  });
}
