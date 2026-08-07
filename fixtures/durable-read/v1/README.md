# Durable read fixture v1

This fixture detects silent durable-format drift: current code must recover the
same public meaning from rows written by the previous committed artifact. Lash
does not migrate these stores across a declared store-schema change; a schema
mismatch is a forward-only reject-and-recreate boundary and therefore fails this
test instead of skipping it.

The fixture format has its own declaration,
`DURABLE_READ_FIXTURE_SCHEMA_VERSION`. CI and pre-commit reject a generated
artifact change unless that declaration changes in the same diff. Store schema
versions remain the authority for whether an old store may be opened.

## Coverage

Every application-owned table in both artifacts has at least one row. Assertions
use supported read/replay surfaces, not row counts, for semantic coverage.

| Durable area | Populated tables | Supported read or refusal asserted |
| --- | --- | --- |
| Session graph and checkpoints | `graph_nodes`, `session_head`/`sessions`, `session_meta`, `blobs`, `usage_deltas`, `runtime_turn_commits` | Ordered graph nodes and every payload field; checkpoint turn, usage, tool, plugin, and execution state; current and legacy receipt replay |
| Session retention | `node_anchors`, `deleted_sessions` | `fork_points`, deletion probe, and typed `SessionDeleted` refusal to reopen a retired id |
| Attachments | `attachment_manifest`, SQLite `artifact_refs`, PostgreSQL `lash_lashlang_artifacts` | Manifest listing plus process-execution-environment reference recovery |
| Receiver queue | `queued_work_batches`, `queued_work_items`, `pending_turn_inputs`, `wake_redelivery_fences`, `session_execution_leases` | Queue/input payloads, deterministic ids, typed wake-rewind refusal, and the raw expired lease generation |
| Processes | `processes`, `process_events`, `process_change_clock`, `process_leases`, `process_observers`, `process_segment_handovers`, `process_tombstones`, `process_wake_deliveries`, `wake_allocation_floors` | Process state; every event payload; observers; continuation; wake delivery/floor; expired raw lease; paginated change feed; typed `ProcessNoLongerRetained` tombstone |
| Triggers | `trigger_subscriptions`, `trigger_occurrences`, `trigger_deliveries`, `trigger_mutation_receipts` | List/filter, delivery reservation, deterministic receipt replay, and `Unchanged` re-registration |
| Effects and awaits | `runtime_effect_replay`, `await_event_meta`, `await_event_waits`, `await_event_revoked_sessions` | Completed effect replay without a local executor, signed await key resolution, and typed late-resolution/revocation behavior |
| Backend metadata | PostgreSQL `lash_schema_versions`; SQLite `user_version` | Exact component/store schema-version comparison before read-back |

The table names above omit PostgreSQL's `lash_` prefix where the logical name is
otherwise identical.

The intentionally expired process and session leases are raw durable generation
facts. Reading them proves decoding and identity continuity; it does not grant
live execution authority. Transient WAL contents, PostgreSQL advisory locks,
database indexes, and database-engine bookkeeping are outside this semantic-read
contract. SQLite WAL files are checkpointed with `TRUNCATE`, required to report
`busy = 0`, and required to be absent before artifact copying.

PostgreSQL generation uses only the dedicated `lash_durable_read_fixture` schema
and never drops or mutates `public`. PostgreSQL owns lease time and effect-row
audit time, so the generator normalizes those volatile timestamps to the fixed
fixture epoch after populating them through supported APIs. Assertions still
read the resulting records through supported surfaces.

## Regeneration policy

Regeneration is deterministic: the generator fixes its clock, signing secret,
lease nonces, trigger incarnation, operation ids, and other identity inputs. Two
consecutive regenerations must produce byte-identical artifacts.

When a read-back test fails, use this decision procedure:

1. If no intentional durable-format change and store-schema bump exists, treat
   the failure as silent drift. Repair decoding/identity compatibility; do not
   regenerate the evidence away.
2. If the on-disk contract intentionally changed, bump every affected store
   schema version and `DURABLE_READ_FIXTURE_SCHEMA_VERSION`, state the
   reject-and-recreate policy, regenerate both backends, and review the semantic
   and artifact diffs.
3. Run the two destructive drift proofs, both normal read-back tests, and the
   no-diff double-regeneration proof before committing.

Generate SQLite:

```sh
LASH_REGENERATE_DURABLE_READ_FIXTURES=1 \
  cargo test -p lash-sqlite-store --test durable_read_fixture \
  regenerate_sqlite_durable_fixture -- --ignored --exact
```

Generate PostgreSQL against a caller-owned throwaway database (Docker is used
only for the pinned `postgres:16-alpine` `pg_dump` client):

```sh
LASH_POSTGRES_DATABASE_URL=postgres://lash:lash@127.0.0.1:55487/lash \
LASH_REGENERATE_DURABLE_READ_FIXTURES=1 \
  cargo test -p lash-postgres-store --test durable_read_fixture \
  regenerate_postgres_durable_fixture -- --ignored --exact
```

Read back without regenerating:

```sh
cargo test -p lash-sqlite-store --test durable_read_fixture
LASH_POSTGRES_DATABASE_URL=postgres://lash:lash@127.0.0.1:55487/lash \
LASH_REQUIRE_POSTGRES=1 \
  cargo test -p lash-postgres-store --test durable_read_fixture
```

For the no-diff proof, hash every non-README file under this directory, run both
generation commands twice, and require the hash set to remain unchanged after
each pass. Released-pin reproducibility starts with the next published alpha;
until then, the committed generators at HEAD are the source of truth.
