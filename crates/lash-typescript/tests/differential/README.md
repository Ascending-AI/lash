# TypeScript differential expectations

`expectations.tsv` is a checked-in Node.js v25.2.1 oracle snapshot. It contains
all 163 Opus review expressions, all 124 sol-sub review expressions, and
259 focused rows for the combined fix findings. Duplicate expressions are
retained so the provenance counts stay executable: the table's 546 rows carry
473 distinct expressions.

Regeneration is deliberate, not part of normal tests. Enum rows are first
transpiled with pinned TypeScript 7.0.2 through `npx tsc --target esnext`, then
executed by the same pinned Node oracle:

```console
node crates/lash-typescript/tests/differential/generate.mjs
```

The generator refuses any Node version other than the stamped version. Review
changes to the inputs and generated table together.
