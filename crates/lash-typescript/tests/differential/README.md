# TypeScript differential expectations

`expectations/<shard>.tsv` is a checked-in Node.js v25.2.1 oracle snapshot, one
shard per `findings/<shard>.txt` of expressions. A row's corpus id is
`differential:<shard>:<n>` with `n` 1-based within the shard, so a new shard's
rows live in their own files and conflict with nothing. Duplicate expressions
are retained so each lane's provenance count stays executable.

To add findings, write `findings/<TICKET>.txt` (one expression per line),
regenerate, then allowlist only that shard's refusals in
`tests/corpus_laws/round_trip_refusals.tsv`, keyed by its ids.

Regeneration is deliberate, not part of normal tests. Enum rows are first
transpiled with pinned TypeScript 7.0.2 through `npx tsc --target esnext`, then
executed by the same pinned Node oracle:

```console
node crates/lash-typescript/tests/differential/generate.mjs
```

The generator rewrites every shard, removes a table whose findings file is
gone, and prints the per-shard and total row counts. It refuses any Node
version other than the stamped version. Review changes to the inputs and
generated tables together.
