# Durable Session and live session are two authorities

## Status

Accepted. Ratified on FIG-3366.

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
