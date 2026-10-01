# Release baseline preparation

`scripts/versioned-surfaces.toml` owns the surface list. The source inventory
resolves each owning constant for the default and synthetic-next builds:

```sh
kiln gate lash my-fork -- python3 scripts/release_baseline.py inventory
kiln gate lash my-fork -- python3 scripts/release_baseline.py check \
  --baseline scripts/release-baseline.toml
```

The draft table declares the cut's values. Every counter starts at 1. String
identities retain their prefix and use version 1. Until FIG-4485 executes, the
check deliberately fails on production values. Missing rows, extra rows,
unresolved constants and unsupported source expressions also fail.

Compare the source inventory with a compiled operator's durable-format table
and version response without a database connection:

```sh
kiln test //crates/lashctl:lashctl__unit_test \
  --test_arg=--exact --test_arg=tests::release_inventory_build_probe \
  --test_arg=--nocapture --test-output-dir .buck2/release-probe
kiln gate lash my-fork -- python3 scripts/release_baseline.py inventory \
  --build-report .buck2/release-probe/root/crates/lashctl/lashctl__unit_test/test.log
```

At the cut, first inspect `release_reset.py --dry-run`. Then run `--apply`
through a private PostgreSQL gate. It resets constants, coupled version pins and admission floors, empties
production catalogs, updates schema references, and runs the schema,
PostgreSQL shape, durable-store and replay-corpus generators. Historical
predecessor captures remain immutable. FIG-4495 owns tagged release capture.
`--source-only` is for a disposable scratch proof and skips generation.

The ignored PostgreSQL catalog and fresh-ledger laws name FIG-4493. Run them
explicitly with `--ignored --exact` during rehearsal. The cut removes their
ignore attributes. SQLite's production-empty and synthetic-adjacent law runs
in both tiers today.

The Python release law is cut-gated under FIG-4485. Rehearse it with
`LASH_RELEASE_CUT=1 python3 scripts/test_release_baseline.py
ReleaseBaselineTests.test_release_values_match_declared_baseline` through
`kiln gate`. At the cut, remove that law's skip gate and update the preparation
gate's selection. The tooling laws run against both the pre-cut and reset trees.

`test_sqlite_stamps_equal_their_catalog_numbers` is cut-gated the same way.
After the reset each SQLite schema stamp must equal the version its
database's compat descriptor writes, in the default and synthetic-next
builds, and every catalog step must lie inside that stamp's range: the
version-bump gate reads a stamp's catalog steps in the stamp's own numbers.
`release_baseline.py check` and `release_reset.py --apply` enforce it, and the
scratch reset law proves it on the reset tree with two red mutants.
