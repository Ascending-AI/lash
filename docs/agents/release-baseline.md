# Release baseline

`scripts/versioned-surfaces.toml` owns the surface list. The release baseline
is one rule over it, `baseline_of` in `scripts/release_baseline.py`: every
counter is 1, and a string identity keeps its prefix and ends in `v1`. There
is no baseline table to keep in step with the registry.

```sh
kiln gate lash my-fork -- python3 scripts/release_baseline.py inventory
kiln gate lash my-fork -- python3 scripts/release_baseline.py check
```

`inventory` resolves each owning constant for the default and synthetic-next
builds. `check` fails on any constant off its baseline, on an unresolved
constant or unsupported source expression, and on a SQLite or PostgreSQL stamp
that differs from the version its compat descriptor writes.

Compare the source inventory with a compiled operator's durable-format table
and version response without a database connection:

```sh
kiln test //crates/lashctl:lashctl__unit_test \
  --test_arg=--exact --test_arg=tests::release_inventory_build_probe \
  --test_arg=--nocapture --test-output-dir .buck2/release-probe
kiln gate lash my-fork -- python3 scripts/release_baseline.py inventory \
  --build-report .buck2/release-probe/root/crates/lashctl/lashctl__unit_test/test.log
```

## The reset

`release_reset.py` moves a tree to the baseline. Inspect `--dry-run`, then run
`--apply` through a private PostgreSQL gate:

```sh
kiln gate lash my-fork -- scripts/ci/with-service.sh pg16 -- \
  python3 scripts/release_reset.py --dry-run
kiln gate lash my-fork -- scripts/ci/with-service.sh pg16 -- \
  python3 scripts/release_reset.py --apply
```

It resets constants, coupled version pins and admission floors, empties the
production migration catalogs, rewrites the `schema.sql` header, renames the
versioned schema files, and runs the generators: the host schemas, the
PostgreSQL shape and teardown artifacts, the durable-read stores, the replay
corpus, the tool-intent journal corpus and the parked-segment golden. It ends
with `kiln sync`. `--source-only` is for a disposable scratch proof and skips
generation.

The reset does not rewrite test pins. A test that asserts a literal version,
an identity hash or a golden byte string fails after the reset and prints the
value the baseline build produces; `docs/release/cut-1.0.md` lists how each
kind is refreshed.

Every golden a generator writes records the generation that wrote it, and its
law refuses a golden of another generation by name. Predecessor captures are
not kept: a release build has no predecessor to read.

After the reset each store stamp must equal the version its component's compat
descriptor writes, in the default and synthetic-next builds, and every catalog
step must lie inside that stamp's range. `release_baseline.py check` and
`release_reset.py --apply` enforce it, and the scratch reset law in
`scripts/test_release_baseline.py` proves it on a reset tree with red mutants.

## The gate

`scripts/ci/version-bump-gate.sh <head> [<base>]` is the strict version-bump
gate. With a base it compares the two commits. Without one its baseline is the
newest `v1`-or-later tag that is not the candidate; before any such tag exists
the candidate must be exactly the release baseline. It is the `version-bumps`
job of `ci.yml`, a required leg of the CI conclusion, and a job the release
workflow's publish step needs. There is no freeze switch and no report-only
exit.
