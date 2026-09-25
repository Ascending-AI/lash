# TypeScript differential expectations

`expectations/<shard>.tsv` is a checked-in Node.js v25.2.1 oracle snapshot, one
shard per `findings/<shard>.txt` of expressions. A row's corpus id is
`differential:<shard>:<n>` with `n` 1-based within the shard, so a new shard's
rows live in their own files and conflict with nothing. Duplicate expressions
are retained so each lane's provenance count stays executable.

To add findings, write `findings/<TICKET>.txt` (one expression per line),
regenerate, then allowlist only that shard's refusals in
`tests/corpus_laws/refusals/<shard>.tsv`, keyed by its `differential:<shard>:<n>`
ids — one file per shard, so a new lane's rows never touch another's.

Regeneration is deliberate, not part of normal tests. Enum rows are first
transpiled with pinned TypeScript 7.0.2 through `npx tsc --target esnext`, then
executed by the same pinned Node oracle:

```console
node crates/lash-typescript/tests/differential/generate.mjs
```

The generator rewrites every shard, removes a table whose findings file is
gone, and prints the per-shard and total row counts. It refuses any Node
version other than the stamped version, pins `TZ=UTC` itself, and evaluates
each row in a fresh realm (`vm.createContext`), so no row can mutate
another's intrinsics and a `Date` row answers the same whatever timezone the
host runs in. Review changes to the inputs and generated tables together.
