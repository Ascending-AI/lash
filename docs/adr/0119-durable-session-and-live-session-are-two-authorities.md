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
about how a session behaves — the code mode execution bounds, Lash VM abilities
and language features, prompt features, discovery operation, output limit,
soft-warning threshold and render; the standard protocol's discovery
operation, `batch` choice and maximum and render — is a creation default. The
session records it in its protocol namespace at creation, and every open,
resume, redrive and process of that session runs under the recorded value, whatever
the opening deployment's factory states (FIG-4398). A child records its
creator-supplied configuration and plugin creation options; parentage copies
none. A fork copies its fork point's recorded configuration. A session-turn
process records its explicitly supplied child environment; an engine or
tool-call process captures its starter's environment (ADR 0088). What a factory
supplies live is physical: the dialect and code or output renderer implementations, the worker service, the
artifact store, deferred-grant resolvers and trace sinks. Each serves the
identity the session recorded or refuses.

The same holds for the core. No open-time option and no core setting is a
behaviour layer over a created session: the core `PromptPlan` is recorded at
creation, and plugins contribute keyed sections (ADRs 0030 and 0133). What a
call asks for and captures — reasoning publication, the fallback output cap, the cache hint and the
response-metadata allowlists — is recorded with the session's model binding
and carried on every request (ADR 0074). The transport an open resolves keeps
only its connection, retry and byte guards. Attachment acceptance is
recorded in each admitted slot; the host store supplies a transient delivery
in an accepted form for each attempt (ADR 0135). The process-tool visibility
filter remains a live host service, separate from attachment acceptance and
delivery. Lash makes no product authorization decision.

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
observers. Runtime administration and wait revocation require their own authority. The
session actor has one owner at a time, and its epoch fence gates execution
writes ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §3).

Sources: `crates/lash/src/durable_session.rs:209` and
`crates/lash-core/src/runtime/durable_queue.rs:160`.

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
revision zero. A failed head read invalidates live-replay continuity instead of
inventing a revision. Existing cursors then recover through their gap path.

The default Live Replay store is process-local. Cross-process visibility
depends on the host's configured replay store. Whichever store is
configured, a session feed's snapshot and every gap replacement are the
durable head (ADR 0002, FIG-5090), so a store decides only which processes'
events reach a feed.

Source: `crates/lash-core/src/runtime/durable_queue.rs:104`.

## A run serves its recorded tool surface

Amended by FIG-6074: tool state, tool loss reports, the tool-source policy
and host tool administration are deleted. An open builds no capabilities
(FIG-4857), and a run records the tool surface it offers when it prepares:
each tool with the provider that serves it, its dispatch surface and the
run's `ToolSelection` of each provider. A resume serves the prompt from that
record and never rebuilds it from the live catalog. Drift is judged per
call, against the recorded tool under its provider's key
(`ToolSurfaceDrift::judge`): a call to a tool its provider no longer serves,
or serves with another dispatch surface, parks typed (`ParkReason::BindingDrift`);
a call to an undrifted tool runs. Which tools a provider offers is that
provider's own config, changed by its owner's typed command or stated for
one run as a run option, never a session-wide authority.

Sources: `crates/lash-core-store/src/tool_surface.rs` and
`crates/lash-core-execution/src/tool_registry.rs`.

### Every host admin read and write needs no activated runtime

A handle's runtime builds capabilities only when a run on it publishes its
plugin transition, and a run may never execute on it: the session never ran,
another process ran it, or a cold reopen's recorded admission does not
validate against this core. So no admin surface reads an activated runtime
(FIG-5139):

- Reads answer from the durable head or the session store: the execution
  snapshot
  (`SessionStateAdmin::snapshot_execution`), the head checkpoint's
  execution root and leaves, `None` when the head records none; the config
  revision and catalog; processes. Host registrations live in host storage
  under ADR 0137.
- Writes are session commands applied at the next transition: appends,
  frame opens, compaction, config transactions, plugin commands and tasks.
- Removed, with no durable meaning: `SessionStateAdmin::restore_execution`
  (execution state moves only with history; seed a fresh interpreter with
  `open_agent_frame`) and `ToolAdmin` with all of its reads and writes
  (FIG-6074: a run's recorded surface is the tool record, and which tools a
  provider offers is its own config).

A plugin query is not an admin read: it runs plugin code over the plugin
view the session's last run or command published, as its head records it.
It runs on the plugins a run built on the handle's runtime, or else on a
private build of that recorded view, which registers, installs and appends
nothing; an open still builds no capabilities (FIG-5344). A store-backed head
records that view as its namespace components and config, never
as a native view copy that every commit would rewrite (FIG-5896). Where the
head records no published view, because the session never ran or commanded, it
is refused with the typed `PluginOperationInvokeError::NotPublished`; a host
publishes one with a session command, such as a tool-catalog refresh, and
retries.

Sources: `crates/lash/src/admin.rs` and
`crates/lash-core/src/runtime/observation.rs`.

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
as a session command that the session actor's owner applies on the runtime
that executes the session's runs (FIG-4202). An open that cannot reconcile
its tools and run turns is never the runtime that runs the session's turns.

Sources: `crates/lash/src/session.rs` (`open_resolved`) and
`crates/lash-core/src/runtime/durable/services.rs` (`RuntimeTurnServices`).

## Consequences

Durable queue access cannot change a run's tool surface or emit runtime
restoration events.
Multiple correctly bound handles can operate beside the serialized shift.
Acquiring a runtime for a poll would perform reconciliation without an
execution need. Inferring execution completion from a queue row disappearing
would confuse claimed, cancelled and completed work; handles and observation
carry the outcome instead.

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
