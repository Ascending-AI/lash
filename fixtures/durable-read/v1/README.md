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

## Two laws: read-back and write shape

Read-back (`*_durable_fixture_reads_with_identical_semantics`) decodes the
committed artifact and asserts the meaning recovered from it. It is blind by
construction to a change in what this build *writes*: a payload field that is
defaulted on read and skipped when absent lets the committed bytes decode,
re-encode, and re-hash exactly as the previous writer wrote them, so the receipt
still replays and every semantic assertion still holds.

Write shape (`*_durable_fixture_expectations_match_what_this_build_writes`)
closes that gap. It re-seeds a throwaway store with the current code and requires
the committed `expected.json` to equal what the seed produces, naming the drifted
JSON paths on failure. Content-addressed identities — process-env refs, node ids,
turn-commit hashes — move with the payload shape, so this law catches shape
changes the read-back cannot see.

The schema-declaration gate is a third, weaker thing: it fires only once a
fixture artifact is already in the diff, so it cannot see a shape change that
leaves `fixtures/` untouched. Order of use: the write-shape law says drift
happened, the decision procedure below decides whether to revert the change or
regenerate for it, and the declaration gate then forces the version bump onto the
regeneration.

### What the write-shape law does not cover

The law compares one artifact: the committed `expected.json`, whose shape is
`ExpectedFixture`. It therefore covers exactly the payloads that struct carries —
runtime commits (session config, graph node payloads, receipt identity), queue
and pending-input identity, process-execution-env specs, await-event keys, and
wake deliveries — plus everything their content-addressed hashes depend on.

It does not cover payloads absent from `ExpectedFixture`. Trigger subscription,
occurrence, and delivery payloads are the largest gap: the read-back assertions
for them are deliberately shallow (subscription key, enabled flag, reservation
status, occurrence payload), so an additive trigger-payload field — FIG-1377's
class of change in a different store — still lands unflagged. Process
registrations are the same shape of gap: `registration_fingerprint` is only
compared against a re-registration by the same build, so it agrees with itself
whatever the payload became.

It also does not cover a new field whose fixture value is skipped during
serialization (e.g. a `None` that serde skips): such a field is invisible to the
law unless a fixture scenario populates it. The `prompt` field was caught only
because it serialized as `Some(empty)`.

Closing those gaps means extending `ExpectedFixture`, which necessarily
regenerates `expected.json` and moves the fixture declaration, so it is follow-up
work on its own ticket rather than something to bundle into an unrelated change.

### Drift these laws missed before the write-shape law existed (FIG-1433)

Both landed with the schema-declaration gate green because neither pull request
touched a file under `fixtures/durable-read/`, and both survived read-back for
the reason above. They surfaced only when FIG-1259 regenerated for an unrelated
attachment-GC schema bump, which is why that regeneration moved the fixture
declaration by two generations (16 to 18) instead of one.

| Commit | Shape change | How it appeared later |
| --- | --- | --- |
| `122e7b348` — Reject drifted process wake delivery payloads (#399, FIG-1377) | `ProcessWakeDelivery` gained the stamped `version` field | `wake_delivery.version: 1` appeared in the SQLite expectations |
| `771e875f2` — Persist session prompts and trace composition changes (#411, FIG-1376) | `PersistedSessionConfig` gained `prompt`, written as an explicit empty layer | `prompt: {}` appeared twice in both backends' expectations, and the `runtime_turn_commits` payload hashes were rewritten |

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
lease nonces, trigger incarnation, operation ids, and other identity inputs.
Determinism is now verified cross-environment (Postgres 14/16/18 and across runner
env/TZ/locale), not just two-consecutive-run on one machine: regenerations must
produce byte-identical artifacts.

An index-only catalog addition is not a reason to regenerate. Indexes are already
outside the semantic-read contract above, and the SQLite catalog is created with
`CREATE INDEX IF NOT EXISTS`, so such an addition does not move a store schema
version: the committed artifact keeps opening and adopts the new index in the
copy under test. Regenerating would only replace working old-file evidence with a
file the current binary just wrote. `sqlite/durable-core.db` therefore predates
the idle-arbitration ordering indexes on purpose.

When a read-back test fails, use this decision procedure:

1. If no intentional durable-format change and store-schema bump exists, treat
   the failure as silent drift. Repair decoding/identity compatibility; do not
   regenerate the evidence away.
2. If the on-disk contract intentionally changed, bump every affected store
   schema version and `DURABLE_READ_FIXTURE_SCHEMA_VERSION`, state the
   reject-and-recreate policy, regenerate both backends, and review the semantic
   and artifact diffs.

When a write-shape law fails, the failure is about the code in your diff, not
about decoding the old artifact, so it gets its own branch:

1. Read the drifted paths the failure names and decide whether writing that shape
   is intended. Most of the time it is not: an accidentally serialized field, a
   flipped skip condition, or a default that stopped being the default. Revert the
   shape change. Regenerating instead absorbs the drift into the committed
   surface, which is exactly the failure mode this law exists to prevent.
2. Only for an intended write-shape change, continue with step 2 of the read-back
   procedure above: bump the affected store schema versions and
   `DURABLE_READ_FIXTURE_SCHEMA_VERSION`, regenerate both backends, and review the
   semantic and artifact diffs — the shape change is now a reviewed surface.
3. Run the two destructive drift proofs, both normal read-back tests, both
   write-shape laws, and the no-diff double-regeneration proof before
   committing.

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
