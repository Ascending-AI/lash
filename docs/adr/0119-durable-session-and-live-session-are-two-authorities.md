# 0119: Durable Session and live session are two authorities

## Status

Accepted.

## Context

Reading or changing durable queues does not require a runtime capable of
executing the session. Opening a runtime reconciles tool sources and protocol
state and materializes plugins. A host that only needs durable input or a
settled read needs an operation that remains valid beside an engine-owned
session drive, without performing that runtime initialization.

## Decision

The session builder has three principal terminal verbs; exactly one creates.

- `core.session(id).create(SessionCreation)` writes the catalog entry and
  initial config head in one store transaction and returns a Durable Session.
  The creation supplies the session spec, relation and plugin options. An
  existing id is `EmbedError::SessionAlreadyExists`, including a matching
  retry; a deleted id is `StoreError::SessionDeleted`.
- `core.session(id).durable()` returns a Durable Session handle. Its first
  operation resolves the existing session; building the handle reads no
  session state. It builds no live runtime, plugin session or tool registry.
- `core.session(id).open()` resolves an existing session and builds the live
  session. A missing id is `EmbedError::UnknownSession`.

The store admission answer decides which racing create succeeds. Hosts that
want create-or-open state that sequence explicitly. Open-time options govern
tool-source policy, enqueue-only mode, process-local plugin factories and the
provider resolver. The resolver must match the recorded provider pin. Config
changes are typed config commands (ADR 0126).

An open session's `session.durable()` reuses the Session Binding's store and
owner-issued ports. Catalog-derived and binding-derived handles share
`DurableSessionOps`, which also implements the runtime's durable queue work.

Sources: `crates/lash/src/session.rs:164`, `:203`, `:254`, `:525`, and `:978`;
`crates/lash/src/durable_session.rs:103`.

### Membership rule

The Durable Session owns operations that remain correct beside an engine-owned
session drive. It exposes `send`, `send_batch`, attachment to accepted input,
root handles and cancellation; pending-input and queued-work reads and
cancels; settled input applications; and `read`, `exists` and `was_deleted`.
User input and non-user queued work remain separate row classes.

The live session owns runtime configuration and views that include the shared
in-memory token ledger. Session deletion uses its scoped delete context and
closure-pin checks. Forking names a destination session and reconciles its
observers. Runtime administration and effect-host wait revocation require
their own authority. Restate serializes session drives; the sealed drive fence
gates execution writes.

Sources: `crates/lash/src/durable_session.rs:209`,
`crates/lash-core/src/runtime/durable_queue.rs:160`, and
`crates/lash-restate/src/session_driver.rs`.

### Acquisition never creates

A catalog-derived handle resolves through `lookup_session`, with successful
acquisition cached in a `OnceCell` shared by its clones. A bound handle reuses
its admitted store. Queue operations refuse an absent or deleted session
before acceptance. A lookup failure stays a typed store error rather than
being reported as absence.

Settled reads answer questions about an id: absence yields `false` for
`exists`, `false` for `was_deleted`, and `None` for `read`; a deletion tombstone
makes `was_deleted` true. These reads write nothing.

Source: `crates/lash/src/durable_session.rs:187` and `:340`.

### Observation

After durable queue success, `DurableSessionOps` publishes `QueueChanged`
best-effort through the configured Live Replay store. Publication failure does
not fail the mutation. The event uses the committed head revision, supplied
by admission or read through `load_session_head_meta`; an empty head has
revision zero. A failed head read invalidates replay continuity instead of
inventing a revision. Existing cursors then recover through their gap path.

The default Live Replay store is process-local. Cross-process visibility
depends on the host's configured replay store.

Source: `crates/lash-core/src/runtime/durable_queue.rs:104`.

## Tool loss at open is a typed fact

### One owner for installing persisted tool state

`install_persisted_tool_state` owns tool-state reconciliation for open, explicit
host restore, persisted-state install and resident re-sync. It returns a
`ToolRestoreReport`; the live runtime retains the report for
`LashSession::tool_restore_report()`. A refused open carries it on the error.

### The report separates three facts

The report distinguishes:

- A lost member has persisted `member: true` and no resolving live source.
- A parked opt-out has no resolving source and persisted `member: false`.
- A replaced identity has a live tool owning its model-facing name under a
  different id. The new identity stays a default member; the old grant and
  opt-out do not transfer.

Only lost members produce a warning. An orphan preserves the host's member
bit; effective membership is `member && !orphaned`, so rebind can restore it.
Alias replacement drops the replaced identity instead of retaining an orphan.

Source: `crates/lash-core-execution/src/tool_registry/rebind.rs:128`.

### Delivery

Open and internal installs retain the report. A non-clean report emits a
`tool_restore.report` trace event with site, authority, policy and the three
classes. A clean report emits no such event. The report is a local runtime
fact, rather than a Session Observation Event or persisted wire shape.

Source: `crates/lash-core/src/runtime/tool_restore.rs:172`.

### Tool-source policy

`ToolSourcePolicy::Tolerate` is the default. `Require` refuses an open with
`SessionError::ToolSourcesUnavailable` when the report has lost members.
Parked opt-outs and replaced identities do not refuse. The core sets the
policy, and the session builder can override it for an open.

Tolerate permits reading and continuing a conversation during tool-source
outages. A deployment whose tool set is part of its execution contract can
choose Require. This policy checks tool restoration, not every condition
needed to run a turn.

#### Only an open may refuse

The installer receives `Open(policy)` or `LiveInstall`. Only the former can
refuse for missing sources. Reconciliation mutates the registry before policy
consultation: a refused open drops the runtime under construction, while a
live install must complete the catalog refresh and retain its report.

A policy refusal makes no config or state commit, restores no protocol, and
emits no `SessionRestored`. It does not promise zero side effects: admitted
load, observer reconciliation and plugin initialization can precede it.

Sources: `crates/lash-core/src/runtime/tool_restore.rs:124` and
`crates/lash-core/src/runtime/lifecycle.rs:239`.

### Opens that will not run a turn

`SessionBuilder::enqueue_only()` selects
`ToolSurfaceOpenMode::PreservePersisted`. The runtime keeps the loaded tool
snapshot without installing it, rebuilding its catalog, changing its tool
generation or producing a restore report. Resident re-sync follows the same
rule. At state-adoption and stamping boundaries, runtime configuration
reasserts preservation, so commits carry the loaded snapshot forward.

Turn entry refuses this mode before admission with
`TurnExecutionRequiresReconciledToolSurface`. Running requires reopening in
default `Reconcile` mode, which restores tools, applies policy and rebuilds
the catalog. Hosts that need no runtime can use `durable()` directly.

Sources: `crates/lash/src/session.rs:142`,
`crates/lash-core/src/runtime/session_api.rs:24`, and
`crates/lash-core/src/runtime/turn_loop/resident_session.rs:383`.

## Consequences

Durable queue access cannot orphan tools or emit runtime restoration events.
Multiple correctly bound handles can operate beside the serialized drive.
Acquiring a runtime for a poll would perform reconciliation without an
execution need. Inferring execution completion from a queue row disappearing
would confuse claimed, cancelled and completed work; handles and observation
carry the outcome instead.

## Model usage accounting

A live session's usage is a durable read (`LashSession::usage()`, async), not
an in-memory ledger, and it answers the same as `DurableSession::usage()` for
the same owner ([ADR 0125](0125-model-usage-is-engine-owned-accounting-delivered-per-call.md)).
