# The Node session oracle

`expectations.json` is a checked-in Node.js v25.2.1 oracle snapshot of the
sessions in `corpus.txt`: ordered cells, each run as a successive classic
Script in one realm (`vm.createContext` plus `vm.Script`), so top-level
`let`/`const`/`class` live in the realm's global lexical environment and
`var`/function declarations on its global object, as ECMA-262 specifies. The
lash side runs every session through the production RLM executor, live and
restarting through the durable snapshot between every pair of cells
(`crates/lash-protocol-rlm/src/testing/cell_conformance/node_oracle.rs`); the
dialect's own view (`tests/corpus_laws/sessions.rs`) holds the corpus
discipline, and every cell also feeds the round-trip law and the artifact
invariants.

Regeneration is deliberate and byte-identical, like the expression table's:

```console
node crates/lash-typescript/tests/differential/sessions/generate.mjs
```

The generator refuses any Node other than the stamped version.

## The mapping

A cell is a Script, observed as:

- its printed lines, under the host printer of deviation register entry 13
  (arguments joined by a space, a plain object or array as compact JSON,
  anything else as ECMA `ToString`);
- how it ended: normally, through `finish(value)` (which ends the cell; a cell
  never catches it), or by an uncaught error of a class — a VM fault's class
  is its `RuntimeError` brand (`runtime-fault-brand`);
- after the cell, one probe per binder name of its session, run as its own
  Script: `console.log(typeof NAME, JSON.stringify(NAME))`. A
  `ReferenceError` answers `unbound` (`tdz` for an uninitialized binding); the
  dialect's static `TS_UNKNOWN_BINDING` is its exact counterpart.

A Script's completion value is not observed: a cell surfaces none. A cell the
dialect rejects statically never enters the realm. Top-level `await` has no
classic-Script meaning, so the corpus holds no await cell.

## The format

```text
session <id>
about <one line>
probe <every binder name the session's cells declare>
deviation closure-boundary          (the one session-wide probe rule)
cell
<TypeScript>
cell reject TS_CODE                 (the dialect refuses it by that code)
<TypeScript>
cell deviation <register slug>      (or: cell defect <open-defect slug>)
<TypeScript>
lash {"outcome": ..., "prints": [...], "probes": {...}}
end
```

A deviation names an entry of the crate README's deviation register; a defect
names its open-defect list. Either states the lash answer (`lash`, or
`lash-resident`/`lash-restart` for one harness mode), which must differ from
Node's — a divergence that closes fails until the answer is deleted — and
ends its session. Content hashes (64 hex digits) in answers are spelled
`<hash>`. Every binder a cell declares is probed, and after every cell the
session's globals must be names its probes answer as bound, so a generated
slot or a block binding that reaches the session fails even unprobed.
