# 0119: Durable Session and live session are two authorities

## Status

Accepted.

## Context

Reading or changing durable queues does not require a runtime capable of
executing the session. Opening a runtime reconciles tool sources and protocol
state and materializes plugins. A host that only needs durable input or a
settled read needs an operation that remains valid beside an engine-owned
session shift, without performing that runtime initialization.

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
want create-or-open state that sequence explicitly. Open-time options are
physical only: the tool-source policy. The recorded model
key binds against the core's registry when a request runs; a key it cannot
serve is `LlmProfileUnavailable`. An open installs
no plugins: the session runs its core's one plugin set under the plugin config
it recorded at creation (ADR 0088). Config changes are typed config commands
(ADR 0126).

A factory is live wiring, not behaviour. What a protocol factory states
about how a session behaves — the RLM execution bounds, Lashlang abilities
and language features, prompt features, discovery operation, output limit,
soft-warning threshold and render; the standard protocol's discovery
operation, `batch` choice and maximum and render — is a creation default. The
session records it in its protocol namespace at creation, and every open,
redrive and process of that session runs under the recorded value, whatever
the opening deployment's factory states (FIG-4398). A child session records
its parent's, and a process started outside any session records the creating
deployment's with its row (FIG-4527). What a factory supplies live is physical: the
dialect and code or output renderer implementations, the worker service, the
artifact store, deferred-grant resolvers and trace sinks. Each serves the
identity the session recorded or refuses.

The same holds for the core. No open-time option and no core setting is a
behaviour layer over a created session: the protocol's prompt config is
recorded at creation (ADR 0030), and what a call asks for and captures — reasoning
publication, the fallback output cap, the cache hint and the
response-metadata allowlists — is recorded with the session's model binding
and carried on every request (ADR 0074). The transport an open resolves keeps
only its connection, retry and byte guards. The host's attachment source
policy and process-tool visibility filter stay live services: they are
host-implemented authority, lash keeps no access or visibility default of its
own beyond open and unfiltered, and it records no access policy for them
because lash makes no authorization decision.

An open session's `session.durable()` reuses the Session Binding's store and
owner-issued ports. Catalog-derived and binding-derived handles share
`DurableSessionOps`, which also implements the runtime's durable queue work.

Sources: `crates/lash/src/session.rs:164`, `:203`, `:254`, `:525`, and `:978`;
`crates/lash/src/durable_session.rs:103`.

### Membership rule

The Durable Session owns operations that remain correct beside an engine-owned
session shift. It exposes `send`, `send_batch`, attachment to accepted input,
run handles and cancellation; pending-input and queued-work reads and
cancels; settled input applications; and `read`, `exists` and `was_deleted`.
User input and non-user queued work remain separate row classes.

The live session owns runtime configuration and live runtime views. Session deletion uses its scoped delete context and
closure-pin checks. Forking names a destination session and reconciles its
observers. Runtime administration and effect-host wait revocation require
their own authority. Restate serializes session shifts; the sealed shift fence
gates execution writes.

Sources: `crates/lash/src/durable_session.rs:209`,
`crates/lash-core/src/runtime/durable_queue.rs:160`, and
`crates/lash-restate/src/session_shifts.rs`.

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

### Only an open that executes hosts a run

A session's runs execute on the most recent open a host holds in this
process, which carries what the host configured on it, or on a runtime the
engine opens itself when no host holds one. Every `open()` and
`open_with_state()` reconciles its tools and can execute, so it may host the
session's runs. `observe_with_state()` is the one open that never executes:
it never registers, and a run admitted while it is the only open runs on the
engine's own runtime (FIG-5091).

There is no enqueue-only open. A host that sends or reads the queue without a
runtime uses `durable()`. A host command needs a live session, and it queues
as a session command that the shift applies on the runtime that executes the
session's runs (FIG-4202). Earlier, an enqueue-only open kept its tools
unreconciled, refused every turn and was still registered as the runtime
that runs the session's turns. A run admitted beside it was therefore refused
terminally.

Sources: `crates/lash/src/session.rs` (`open_resolved`) and
`crates/lash/src/core/session_shifts.rs` (`shift_runtime`).

## Consequences

Durable queue access cannot orphan tools or emit runtime restoration events.
Multiple correctly bound handles can operate beside the serialized shift.
Acquiring a runtime for a poll would perform reconciliation without an
execution need. Inferring execution completion from a queue row disappearing
would confuse claimed, cancelled and completed work; handles and observation
carry the outcome instead.

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.
