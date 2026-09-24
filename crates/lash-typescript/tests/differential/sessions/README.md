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

The generator refuses any Node other than the stamped version. The mapping
below is written once, in code, in `realm.mjs`, which both this corpus and the
generated sessions are answered through.

## The mapping

A cell is a Script, observed as:

- its printed lines, under the host printer of deviation register entry 13
  (arguments joined by a space, a plain object or array as compact JSON,
  anything else as ECMA `ToString`);
- how it ended: normally, through `finish(value)` (which ends the cell; a cell
  never catches it), or by an uncaught error of a class — a fault in an
  operation ECMA-262 specifies to throw is that ECMA class, and a VM fault
  with no ECMA counterpart is its `RuntimeError` brand
  (`runtime-fault-brand`);
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
host <name> <JSON>                  (optional, repeatable: a host binding)
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

A `host` line gives the session a read-only binding its host supplies. Node
sees a plain, non-writable global holding the JSON value; the lash side binds
it as a lazy projection the host re-resolves after every restart, which is how
a host projects data it owns (a document, a record) into a session.

A deviation names an entry of the crate README's deviation register; a defect
names its open-defect list. Either states the lash answer (`lash`, or
`lash-resident`/`lash-restart` for one harness mode), which must differ from
Node's — a divergence that closes fails until the answer is deleted — and
ends its session. Content hashes (64 hex digits) in answers are spelled
`<hash>`. Every binder a cell declares is probed, and after every cell the
session's globals must be names its probes answer as bound, so a generated
slot or a block binding that reaches the session fails even unprobed.

## Generated sessions (FIG-3608)

The hand-written corpus checks the divergences someone thought of.
`generated.json` checks sessions nobody wrote: a seeded generator
(`crates/lash-protocol-rlm/src/testing/cell_conformance/node_oracle/generator.rs`)
draws multi-cell sessions from the dialect's accepted grammar — bindings at
every scope, shadowing, closures, every loop form, the exotic built-ins,
object and array mutation, reassignment within a cell and through
`globalThis` across cells, census-rejected cells, cells that throw or finish.
Every construct names the census row (which must be `accepted`) or the WHATWG
URL surface it draws from, and every cell is also plain JavaScript, so Node
runs it as written. The generator is a pure function of its seed.

The bounded corpus is the first seeds; `generated.json` holds each session
with Node's answer, written through the oracle service `oracle.mjs` by one
deliberate, byte-identical step:

```console
kiln run //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test -- \
    --ignored --exact testing::cell_conformance::node_oracle::generated::write_the_generated_corpus
```

The cacheable test partition regenerates every session from its seed and
requires it to be the one checked in, so a generator change is a deliberate
corpus change, then runs it live and reloading between every pair of cells
against Node's answer under the `closure-boundary` rule. Longer runs draw fresh
seeds and ask Node live:

```console
LASH_GENERATED_SEEDS=START..END kiln run //crates/lash-protocol-rlm:lash-protocol-rlm__unit_test -- \
    --ignored --exact testing::cell_conformance::node_oracle::generated::generated_sessions_against_live_node
```

The manual fuzz run (`fuzz-smoke` in CI) runs it on seeds derived from its run
id. Each divergence is minimized — later cells, earlier cells and top-level
statements deleted while the divergence stays the same one — and printed as
the row of this corpus that pins it. A defect small enough to fix is fixed
with its row; a larger one is registered in the crate README's open-defect
list with its row and a ticket, and the generator stops drawing its shape,
naming the entry, until it is fixed.

## The snapshot round-trip law (FIG-3608)

For every value type the dialect accepts, a value created in one cell, stored
in a session global, reloaded from the durable snapshot and used in the next
cell behaves as if it had never been stored: as the same code in a single
cell, which must itself be Node's answer (the round-trip rows of
`generated.json`). The rows cover every heap object kind the VM has, which
`lashlang::testing::heap_object_kinds` names by an exhaustive match, and the
primitives. A type that cannot round-trip is refused with a named diagnostic;
a row whose law fails today is pinned by the open defect or registered
deviation that breaks it, in the harness modes it fails in, and fails once
that is fixed.
