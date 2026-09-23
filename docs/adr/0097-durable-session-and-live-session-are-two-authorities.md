# Durable Session and live session are two authorities

## Status

Accepted. Ratified on FIG-3366; extended on FIG-3367 with the tool-restore
report and the tool-source policy.

Amended 2026-09-23 (FIG-3540), **not yet implemented**: under [ADR 0101](0101-one-session-ingress-carries-every-admitted-item.md) the handle's
pending-input and queued-work reads, cancels and abandons become one set over
Session Ingress items, and batch ids become item ids, with no aliases. The split
between the two authorities is unchanged.

## Context

Reaching a session's durable queue required `LashSession`, and the only way to
get one was `core.session(id).open()`. Open builds a whole runtime: it
reconciles persisted process-observer intents, admits and leases the store,
materialises every plugin, restores the tool registry and protocol state,
emits `SessionRestored`, and admits the session's pending processes. A host
that only wanted to list pending turn input paid all of it. On a core whose
plugin stack does not carry a persisted session's tool sources, it also
orphaned every persisted tool and warned — once per poll (FIG-3353).

The shape was the defect. `LashSession` fused two authorities: the operations
that need a core able to *run* this session, and the operations that are
answerable from its store and stay correct while another process holds the
Session Execution Lease. Nothing in the types told a host which was which.

The split was already under way and had drifted. `LashCore::enqueue_turn_input`
("persist host input without opening a competing session writer"),
`read_session`, `session_exists` and `session_was_deleted` had each been lifted
onto `LashCore` one at a time; list and cancel never were. The lifted enqueue
had diverged from the live one in two ways that mattered: it published no
`QueueChanged` event, and it acquired its store with `create_store`, which
writes session metadata on every backend — so enqueueing to an id that had
never been created silently materialised a session.

## Decision

The session builder has three terminal verbs, and exactly one of them creates.

* `core.session(id).open()` is unchanged and yields the **live session**.
* `core.session(id).durable()` yields a **Durable Session**: no Session
  Execution Lease, no plugin session, no tool registry, no lifecycle events, no
  observer-intent reconcile, no process admission. It never creates.
* `core.session(id).create()` writes the session's catalog entry — with exactly
  the policy and relation `open()` would have used — and returns its Durable
  Session. It builds no runtime either. It is idempotent, preserving the
  metadata and Session Relation an existing id already carries, and refuses a
  deleted id with the store's typed `SessionDeleted`.

The Durable Session owns every operation that is correct beside another
process's writer: enqueue turn input (validation, driver wake and receipt
preserved), `pending_turn_inputs`, the three pending-input cancels,
`queued_work`, `cancel_queued_work_batch`, both `abandon_*_claim`,
`turn_input_applications` and its remote form, and the settled `read` /
`exists` / `was_deleted`. They no longer exist on
`LashSession`, and `LashCore::{enqueue_turn_input, read_session,
session_exists, session_was_deleted}` no longer exist. There are no forwarding
shims and no deprecated aliases.

An open session reaches the same operations through `session.durable()`. That
handle is derived from the session's Session Binding: it reuses the binding's
admitted store and owner-issued ports and never manufactures a catalog, so an
exact binding keeps today's optional-capability errors (`MissingSessionStore`,
`SessionCatalogUnavailable`). Both constructions share one implementation per
operation (`lash_core::facade_support::DurableSessionOps`), which the live
`RuntimeHandle` also routes its queue methods through, so a queue mutation
behaves identically whichever handle issued it.

### Membership rule

An operation belongs to the Durable Session when it is answerable from the
session's store and remains correct while another process holds the Session
Execution Lease. Everything else stays on the live session or on Session
Administration:

* `delete` needs a scoped `SessionDeleteContext` and closure-pin checks.
* fork names the id as the *destination* and reconciles process observers.
* live `usage_report` and `read_view` include the in-memory shared token
  ledger, which no store read can reproduce.
* `revoke_durable_waits` uses the binding's effect host, not its store.
* all of `SessionAdmin` drives a live runtime.

`await_queued_work_batch` is **not** part of the handle and was deleted rather
than moved. It polled `queued_work()`, which hosts already have; its answer —
"no longer pending" — becomes true as soon as a claim hides the row, before the
work has run; and it returned the same `()` for drained, cancelled and
never-existed. The question it looked like it answered ("did my queued command
take effect") is answered by the Session Observation stream and the read view,
which are event-driven and carry the outcome.

### Acquisition never creates

`durable()` resolves an existing store through the catalog's non-creating seam
(`open_existing_store_by_id`), at most once per handle and shared by its
clones; `create_store` is unreachable from a Durable Session. Every queue
operation therefore requires a session id the store already knows. Enqueueing
to an id that was never created is `EmbedError::UnknownSession`; to a deleted
one it is `StoreError::SessionDeleted`. Nothing is stored and no driver is
woken. This is a deliberate behaviour change from `LashCore::enqueue_turn_input`,
which materialised metadata: a host that enqueued before a first open now
creates the session first, with `create()`.

The three settled reads are an exception in *reporting*, not in authority.
`exists`, `was_deleted` and `read` exist to answer a question *about* an id, so
an unknown id is their answer — `false`, `false`, `None`, exactly the outcomes
the removed `LashCore` methods returned — and they write nothing either way.

### Observation

Queue mutations publish `QueueChanged` through the core's Live Replay
publisher, best-effort and only after durable success; a publication failure is
warned and never fails the mutation. The published revision is the committed
head read back from the store (`load_session_head_meta`), not
`read_session_state_version`, which is an encoding marker: a cursor minted from
the marker corrupts reconnect. A store with no committed head publishes at the
defined empty-head revision, zero.

There is no new observation hub. Cross-process visibility of these events is
therefore exactly the property of the configured Live Replay store, which is
in-memory — and so process-local — by default.

## Consequences

Polling a session's queue no longer builds a runtime, so it cannot orphan a
persisted tool, cannot emit `SessionRestored`, and cannot admit processes. The
FIG-3353 poll costs one store query.

Hosts that relied on enqueue materialising a session must create it first. The
in-repo callers are migrated with this change; external adopters see a typed
`UnknownSession` rather than a session that quietly appears.

Two correctly bound stores may address one session at once — that was already
true and is now first-class. Replacing a binding's owner services is still
forbidden, which is why the binding-derived handle reuses the binding's ports
instead of consulting the core.

## Extension (FIG-3367): tool loss at open is a typed fact, with one owner

The Durable Session removed the *reason* a poll orphaned tools. It did not
answer the other half of FIG-3353: what a host is told when an open really does
lose them. `ToolRestoreReport` said hosts should surface orphans, and no host
could — cold open and persisted-state install turned it into a `tracing::warn!`,
resident re-sync discarded it, and only the explicit host restore returned it.

### One owner for installing persisted tool state

`install_persisted_tool_state` in `lash-core` is the single site that reconciles
a persisted `ToolState` onto a session's registry. The four constructions that
install — `from_host_state` (every builder open, resume, managed-child
materialise, queued-work rebuild, remote host open), the host `restore_tool_state`,
the persisted-state install, and the resident re-sync — call it and none logs
and drops. Reconcile semantics, the generation rule and the persisted encoding
are unchanged: the owner classifies what reconcile already decided.

### The report separates three facts

An unresolved persisted tool id is one of three things, and only the first is
capability loss:

* **Lost member** — persisted `member: true`, no live source resolves the id.
* **Parked opt-out** — unresolved, `member: false`. The host had already turned
  it off; nothing it could use is missing.
* **Superseded identity** — a live id owns the old id's model-facing name. The
  capability is present under a new identity, which is a default member, and the
  old grant does not transfer.

Only lost members log at warn. Before this, an opt-out counted as loss and a
replacement was silently invisible, so a refusal built on the old list would
have rejected intentional opt-outs and waved replacements through.

### Delivery

* **Open** keeps the report on the runtime; the facade reads it as
  `LashSession::tool_restore_report()`. A refused open carries it on the typed
  error instead.
* **Internal reloads** (resident re-sync, persisted-state install) replace that
  same retained report and emit a `tool_restore.report` trace event naming the
  site, the policy and all three classes.

The report is deliberately *not* a Session Observation Event: that enum crosses
the remote protocol as `RemoteSessionObservationEvent`, and the report is not
wire state. Nothing about the report reaches `REMOTE_PROTOCOL_VERSION` or any
persisted encoding.

### Tool-source policy

`ToolSourcePolicy` is set at core assembly (`LashCoreBuilder::tool_source_policy`),
overridable per open (`SessionBuilder::tool_source_policy`), and carried on the
runtime host config so runtime-initiated constructions honour it.

* **Tolerate** (the default) — the session opens and the report is delivered.
* **Require** — the open refuses with `SessionError::ToolSourcesUnavailable`
  when the report has lost members. Parked opt-outs and superseded identities
  never refuse.

Tolerate is the default because locking a user out of a conversation is worse
than degrading it: a chat whose MCP server is down is still worth reading and
often still worth continuing. Unattended and fixed-tool deployments — a queued
worker, a scheduled agent, a service whose tool set is part of its contract —
set Require, where running without a tool silently is the worse failure. There
are two values on purpose: per-tool "required" declarations wait for a host that
needs them, and Require is not advertised as a complete runnability check.

#### Only an open may refuse

The policy is an *open* policy. The three installs onto an already-live runtime
— the host's `restore_tool_state`, a persisted-state install, and the resident
re-sync — always tolerate, retain the report and return it, on a Require core
as much as a Tolerate one.

That follows from the mutation order rather than from taste. The installer
commits the reconciled surface before any policy is consulted, so every refusal
is a refusal *after* the registry changed. At open that is safe and deliberate:
the runtime being built is dropped with the error and nothing the host can
reach ever observed it. On a live runtime the same refusal would skip the tool
catalog refresh, the plugin-state stamp and the report retention, leaving the
session holding a registry and a catalog that disagree — and, in the resident
re-sync's case, failing a mid-turn reload because an MCP server went away,
which is the exact degradation Tolerate-by-default exists to absorb, arrived at
at a moment nobody chose to open anything. The type says so: the installer's
authority is either `Open(policy)` or `LiveInstall`, and `LiveInstall` has
nowhere to put a policy, so a future install site cannot acquire the power to
refuse by passing one.

A refusal promises: no config or state commit, no protocol restore, no
`SessionRestored`, and a released Session Execution Lease (a following open
acquires it). It does not promise zero side effects. By the time tool state is
installed, the observer-intent reconcile has run, the admitted load has claimed
and released its lease, plugins have materialised and `initialize_session` has
run. A zero-side-effect refusal would need a separate preflight contract.

### What an orphaned commit leaves durable

FIG-3353 asked whether a commit taken while tools are orphaned persists them as
non-members for good. It does not. An orphan keeps the host's `member` bit,
effective membership is derived (`member && !orphaned`), and rebind restores it
against the live manifest. The orphan flag and the catalog generation are the
only durable trace. Alias replacement is the exception: a superseded identity is
dropped rather than orphaned and its opt-out does not transfer to the new id.

### Opens that will not run a turn (FIG-3353, continued)

A *reconciling* open is still the wrong tool for a host that only wants to
commit durable input on a runtime — e.g. a worker that opens a session to
append pending input on a core that does not carry the session's tool sources.
`ToolSurfaceOpenMode` states which open it is:

* **Reconcile** (the default) installs the persisted `ToolState` and rebuilds
  the catalog exactly as before.
* **PreservePersisted** declares the open will not run a turn. The persisted
  snapshot is not installed, the catalog is not rebuilt, no generation bumps,
  no `ToolRestoreReport` is produced and the lost-tools warning does not fire —
  for an intentional no-source open the warning is absent, not merely
  downgraded. The runtime keeps the loaded snapshot on its state rather than
  restamping it from an unreconciled registry, so any commit the open takes
  carries the persisted surface forward untouched: no orphans, no generation
  movement, and the tools are still catalog members on the next reconciling
  open. The same skip applies to the resident re-sync on such a runtime.

The declaration is a fence, not merely a claim. Every turn-execution entry —
direct turns, queued and prepared drives, and the shared logical-turn funnel
— refuses a `PreservePersisted` open with
`RuntimeErrorCode::TurnExecutionRequiresReconciledToolSurface` before
admission, because the surface was never reconciled and no `ToolSourcePolicy`
was enforced: letting a turn run there would both execute against an
unreconciled registry and bypass `Require`. Reopening in `Reconcile` mode is
the escalation path; it performs the restore, applies the policy and rebuilds
the catalog.

Preservation is owned by the runtime's open configuration, not only by the
resident state. Whole-state replacements — resident reload, append-receipt
replay, append rollback — rebuild the state wholesale, so the runtime
reasserts the preservation marker from `tool_surface_open_mode` at each
adoption and at every stamp boundary; a `PreservePersisted` commit therefore
carries the loaded snapshot forward even across those replacements.

The facade exposes it as
`SessionBuilder::enqueue_only()`; below the facade it rides the runtime host
config (`RuntimeControlConfig::tool_surface_open_mode`), so every construction
the open performs sees the same choice. Hosts that need durable input without
a runtime at all should still prefer `durable()`, which builds nothing.
