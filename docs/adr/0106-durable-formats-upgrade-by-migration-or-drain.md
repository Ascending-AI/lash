# 0106: Durable formats upgrade by migration or drain after the clean-slate release

## Status

Proposed 2026-09-25 (FIG-3660). This record takes effect at the clean-slate
release (arc FIG-3789). Until then, the temporary rule holds: a durable-format
change is a version bump plus a typed refusal of older state. At the release,
this record replaces that rule where it is written as current policy: ADR 0043
(*journal prefix*), ADR 0045 (FIG-3588), ADR 0105 §12, and the docs of
`state_version.rs`, `lash::formats`, `durable_read_fixture.rs` and
`PostgresStorage::schema_version`. None of this record is implemented yet.

## Context

figments upgrades by rolling deploys under live traffic. Old and new workers
share one PostgreSQL store and one Restate cluster. On `origin/main`
`b85e109ae`:

- `versioned-surfaces.toml` registers 68 surfaces. Each `Comparable` decoder
  accepts exactly one version and refuses both older and newer versions.
  `OLDEST_SUPPORTED_SESSION_STATE_VERSION` exists, but admission still requires
  an exact match.
- PostgreSQL opens only when the component stamp (131) matches exactly.
  `SCHEMA_MIGRATIONS` exists, but no arm in it targets the current component.
  PostgreSQL and SQLite both reject a mismatched store and expect it to be
  recreated.
- `drain_status` counts across the whole store. Under a mixed fleet it never
  reads drained.
- `BuildGeneration` (ADR 0105 §7) is not used anywhere. The journal-prefix fork
  (ADR 0043) is not implemented.
- Restate pins an invocation to its deployment once the invocation has started.
  New invocations go to the latest deployment, and that includes segment
  successors and group children of older state. Object state, such as the
  durable-wait and effect-group indexes, is shared by every deployment.

## Decision

### 1. Deployment model

**Drain generation `G`** is a digest of the versions that give a journal its
meaning (the D rows in §2). Hosts keep registering one Restate deployment per
build (ADR 0043), so in-flight invocations drain on the deployment that
started them. Cross-invocation successors, such as handovers, group children
and redrives, carry their `G`. They route to handlers served under
generation-suffixed service names, which keeps a successor reachable on the
old deployment. Most releases do not change `G`, and for those a rolling
deploy needs nothing extra. When `G` does change, the old deployment stays
registered until `drain_status(G_old)` reads drained.

**Fleet format version `F`** is a single row in the PostgreSQL store.

- Each build declares the range `[min_F, max_F]`. It reads every stored
  version in that range and writes at the active `F`.
- During the roll, new workers read both versions and write the old one.
- When every live worker reports `max_F ≥ target`, the operator runs
  `lash admin finalize-upgrade` and writes switch to the new version. Rollback
  is safe up to that point.
- A build whose range does not contain the active `F` refuses to start, with a
  typed error, before it takes any traffic. This is the only refusal left on
  the upgrade path.
- SQLite runs in a single process, so it finalizes on open.

### 2. Per-surface policy

The three policies are:

- **M**: forward migration. Schemas take expand/contract DDL, and payloads go
  through an upcaster chain on read.
- **D**: drain by `G`.
- **C**: coexistence, with both versions live during the window.

| Surface | Persisted in / read by | Policy |
|---|---|---|
| PG `SCHEMA_VERSION` | `lash_*` tables; every worker at open | **M**. An operator pre-deploy job runs it (lash ships the command). Changes are expand-only, and contract lands one release later. The stamp carries `oldest_reader`. |
| SQLite four `*SCHEMA_VERSION` | `user_version` per file | **M** on open, in one transaction, after a backup. |
| `CURRENT_SESSION_STATE_VERSION` | session marker; lease admission | **M** at rest: the session is upcast at admission after finalize. **D** in flight: a claimed or parked turn keeps its generation until it settles. The window is `[OLDEST_SUPPORTED, CURRENT]`. |
| `SESSION_HEAD_META`, `PROTOCOL_TURN_OPTIONS`, `PARENT_SCOPE_STORAGE_PAYLOAD`, `PROCESS_WAKE_DELIVERY_FORMAT`, `NATIVE_DRIVER_STATE` | mutable rows; any worker | **M**. Upcast on read, then write at `F`. |
| `SESSION_NODE_BODY`, `RUNTIME_COMMIT_RECEIPT`, `SESSION_CHECKPOINT`, `CHECKPOINT_COMPONENT_ENCODING`, `RLM_SNAPSHOT`, `LASHLANG_SNAPSHOT`, `HEAP_SIZE_SCHEDULE`, `NATIVE_TRANSPORT`, `PROCESS_EVENT_VOCABULARY` | immutable, hash-addressed history; replay and reopen | **M, read-only**. Upcast on read and never rewrite the stored bytes. The upcasters are permanent. |
| `WORKFLOW_GRAPH_SCHEMA`, `WORKFLOW_TYPE_FACET` | derived projection | **M**. Regenerate from the module. |
| `LASHLANG_SEMANTIC_HASH`, `BYTECODE_FORMAT`, four request-identity encodings, `*_FAMILY_VERSION`, `FRAME_KEY`, `JOURNAL_IDENTITY` | content addresses, idempotency keys | **C**. New identities are minted under the new family after finalize. A stored identity is never re-derived, and a retry is verified under the family the stored identity names. |
| `EFFECT_JOURNAL`, `RESTATE_PROCESS_JOURNAL`, `PROCESS_COMMAND_JOURNAL_PAYLOAD`, `DURABLE_WAIT_REQUEST`, `TOOL_CHILD_REQUEST`, `TOOL_SETTLEMENT`, `TOOL_ATTEMPT_CAPTURE`, `TOOL_PRESENTATION` | Restate journal and inputs; replay | **D** |
| `LASHLANG_CELL_JOURNAL_GRAMMAR`, `LASHLANG_REPLAY_KEY_GRAMMAR`, `INSTRUCTION_ACCOUNTING`, `LASHLANG_VM_ABI` | grammar the journals were written under | **D** |
| `TURN_CHECKPOINT_SCHEMA`, `VM_CONTINUATION_FORMAT`, `LASHLANG_SEGMENT_STATE` | parked turns and handovers (PG, Restate input) | **D**. The successor routes to its own `G`. |
| `DURABLE_WAIT_INDEX_IDENTITY_EPOCH`, `EFFECT_GROUP_INDEX_PROTOCOL_VERSION` | shared Restate object state | **C**. Object keys are namespaced by version, and deliverers fan out until the old namespace drains. Stop-the-world epoch bumps are no longer allowed. |
| `REMOTE_PROTOCOL_VERSION`, `PROCESS_CURSOR_VERSION`, `TRACE_SCHEMA_VERSION` | live wire, host cursor, trace readers | **C**. Peers negotiate or accept `[N-1, N]`. |
| `PROCESS_LEASE_SCHEMA` | SQL engine lease | Deleted before the release (FIG-3667/3668). If it survives the release, it is **M**. |
| Durable-read fixtures, `tool_intent_journals/`, replay corpus | tests | These become the frozen upgrade corpus (§4). |

Out of scope, because nothing durable carries them: `TOOL_CHILD_REBIND`,
`SOURCE_CACHE` and `QUEUED_WORK_CLAIM_LEASE_ENCODING`.

Why: a journal replays by re-executing code, so only its own code can replay
it. Both versions read shared rows during a roll, so writes switch at finalize.
Hash-addressed history cannot be rewritten without moving its addresses.

### 3. What stays fail-closed (typed)

Some state still fails closed, with a typed error:

- **State outside the read window.** The supported upgrade is from release N-1
  to N. A startup preflight refuses a skipped release.
- **A version newer than the build.** Finalize prevents this in normal
  operation.
- **Integrity failures:** hash, signature and divergence.
- **A journal whose `G` no build serves.** It parks (ADR 0045 `Parked`). It is
  never destroyed.

Some invocations never drain, such as a process parked on a long wait. When
their window closes, the operator either forks them onto the new `G` (ADR
0043) or cancels them to a typed terminal state. The recorded `version()` op
of ADR 0105 §7 is not used for format changes.

### 4. Lifecycle, tests and gates

**Change lifecycle.** `check_version_bumps.py` still requires a bump for every
format change. The author of the bump also supplies the upgrade path:

- **M:** a migration arm, or an upcaster from the previous version.
- **D:** nothing extra, because `G` is derived.
- **C:** the dual-read window.

The release notes then list the DDL to apply, whether a finalize is needed, and
whether `G` changed. A changed `G` means the old deployment has to stay
registered.

**Upgrade laws.** Every surface has a law for every supported predecessor. Each
law starts from a frozen fixture captured at the previous release tag:

- **M and C:** new code reads or upcasts the fixture to the same public
  meaning. This extends the durable-read law.
- **D:** new code parks a journal from a foreign `G` with zero dispatch, and
  the release binary replays that journal to completion.

These laws run on every PR.

**Mixed-version rolling E2E.** This replaces `version-bump-recreation-e2e.sh`.
The harness brings up Restate, PostgreSQL, the release-tag image and head. It
seeds live turns, parked processes, effect groups and triggers on N, then rolls
half the fleet under traffic and checks:

- no refusals;
- no duplicate effects;
- both builds read each other's rows before finalize.

It then rolls back and forward again, finalizes, waits for `drain_status(G_old)`
to drain, and retires the old deployment. The E2E runs on every PR that touches
a registered surface, and nightly.

**Gates.** Each surface in `versioned-surfaces.toml` declares
`upgrade = migrate|drain|coexist`. A new `check_upgrade_paths.py` checks every
bump:

- An M or C bump needs its predecessor fixture, plus an upcaster or migration
  arm from a registry that a test enumerates.
- A D bump needs its `G` input.

`check_version_bump_fixtures.py` requires a `SCHEMA_MIGRATIONS` arm from the
release component. An arm may drop an object only when no build in the window
reads it.

**Deprecation.** A reader for an old format stays for at least one release
after writes stop, and it is removed only when the deep `FormatProbe` walk
finds zero rows. History upcasters are permanent.

### 5. Must land before the cut

1. Derive `G`. Stamp it on process rows, turn claims, parks, handovers and
   group children, and wire `BuildGeneration`.
2. Add `drain_status(G)`, reported per generation.
3. Serve generation-suffixed service names and route successors by `G`.
   Without this in the release, the first change to `G` becomes a naming
   cutover.
4. Add the `F` row, per-worker range heartbeats, finalize, and a startup
   preflight.
5. Give every `Comparable` decoder a read range (`[V, V]` at first) and an
   upcaster hook. Session admission uses the window constant.
6. Add a PostgreSQL `oldest_reader` stamp and a range open. Add a SQLite runner
   that migrates on open.
7. Audit that stored identities carry their family tag, and that
   `session_ingress` and turn-park payloads carry a version.
8. Add remote-protocol negotiation.
9. Namespace Restate object state by version.
10. Build a fixture-capture tool and run it at the cut into
    `fixtures/upgrade/<release>/`.
11. Build the rolling E2E harness.
12. Implement the journal-prefix fork. This can land after the cut, but before
    the first change to `G`.
13. Sweep the docs listed under *Status*.

## Consequences

Most format changes cost an upcaster and a fixture. Execution-semantics changes also cost a drain window.
