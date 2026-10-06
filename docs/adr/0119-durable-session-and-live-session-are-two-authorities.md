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
depends on the host's configured replay store. Whichever store is
configured, a session feed's snapshot and every gap replacement are the
durable head (ADR 0002, FIG-5090), so a store decides only which processes'
events reach a feed.

Source: `crates/lash-core/src/runtime/durable_queue.rs:104`.

## Tool loss is a typed fact of the run that restores tool state

An open builds no capabilities (FIG-4857): a session's tool registry and
plugins exist only once a run publishes its plugin transition. Tool loss,
`ToolSourcePolicy` and host tool administration therefore belong to runs and
to durable state, never to the open (FIG-5134).

### One owner for installing persisted tool state

`install_persisted_tool_state` owns tool-state reconciliation for a run's
construction of the session's capabilities, a host restore command, a
persisted-state install and a resident re-sync. It returns a
`ToolRestoreReport` and never refuses. The runtime keeps the report until its
next turn starts, which reports a non-clean one as
`TurnEvent::ToolRestoreReported` right after `TurnStarted`. The sender reads
it on the run's output (`TurnOutput::tool_restore_report`), and observers read
the same typed event on the session's observation feed.

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

A non-clean report also emits a `tool_restore.report` trace event with its
site and the three classes. A clean report emits neither the trace event nor
the turn event.

Source: `crates/lash-core/src/runtime/tool_restore.rs`.

### Tool-source policy

`ToolSourcePolicy::Tolerate` is the default. Under `Require`, a turn run's
recorded plugin transition previews the restore over the capabilities it
built and refuses when it would lose a member, before the transition
publishes anything. The refusal is the run's typed terminal,
`RuntimeErrorCode::ToolSourcesUnavailable` with the report as its
`RuntimeErrorCause`, which the sender reads as `SendOutcome::Refused`.
Parked opt-outs and replaced identities do not refuse. The refused run makes
no config or state commit, restores no protocol session and emits no
`SessionRestored`. Command runs tolerate, so a host's tool restore still
applies on a `Require` core. The core sets the policy, and the session
builder can override it for the runs an open hosts.

Tolerate permits reading and continuing a conversation during tool-source
outages. A deployment whose tool set is part of its execution contract can
choose Require. This policy checks tool restoration, not every condition
needed to run a turn.

Sources: `crates/lash-core/src/runtime/tool_restore.rs` and
`crates/lash-core/src/runtime/shift/plugin_transition.rs`.

### Host tool administration is durable

`ToolAdmin::state` reads the tool state the session's durable head recorded,
beside the `ChangeToolState` commands a host submitted that no run applied
yet, typed as pending; it builds nothing. A membership change, snapshot apply
or restore is a `SessionCommand::ChangeToolState`: refused at once with a
typed `ReconfigureError` when it is invalid against the recorded snapshot,
otherwise applied in lane order by the command run against the capabilities
its transition built, and settled as a typed `ToolStateChangeOutcome`. A
protocol session extension is durable the same way: its session nodes are a
host append the command lane applies, which the protocol replays on every
restore.

Sources: `crates/lash/src/admin/tool_state.rs` and
`crates/lash-core/src/runtime/tool_state_commands.rs`.

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
