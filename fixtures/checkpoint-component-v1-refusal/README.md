# Checkpoint component encoding v1 refusal fixtures

These fixtures retain the last durable checkpoint produced with component
encoding version 1. They are intentionally not regenerated when the current
durable-read fixture advances.

- `sqlite/durable-core.db` uses the schema-38 SQLite catalog while
  retaining the checkpoint blob from commit `a461383be`. Its one live head has
  all six manifest-component edges projected in `checkpoint_blob_refs`; the
  fixture was re-armed through the same transactional 37 -> 38 backfill as a
  deployed legacy catalog.
- `postgres/fixture.sql` uses the schema-61 PostgreSQL catalog while
  retaining the checkpoint blob from the same commit.

Both contain the `durable-read-fixture` session with an `execution_state`
checkpoint component stamped at encoding version 1. Store-schema bumps may
refresh the surrounding catalog so the current binary can reach hydration, but
the retained checkpoint blob stays byte-for-byte at component encoding 1. The
current binary must then refuse that session with the exact drain-and-recreate
diagnostic. These are rejection fixtures, not compatibility fixtures: never
update their component version.
