# Protocol Capabilities Enter Through the Plugin Contract

## Status

accepted

## Decision

A protocol acquires runtime capabilities only through the uniform plugin contract — never through facade-level special wiring. Two contract extensions make this hold for RLM, the one protocol that needed more than a driver slot:

1. **Plugin-contributed Process Engines.** `PluginFactory` gains a host-level contribution method mirroring `extension_contributions()`: after the plugin host is built, core asks each factory for Process Engine registrations, passing a read-only host context (built plugin extensions, trace context, host capabilities such as process-lifecycle availability). Each registration pairs the world-bearing execution engine with a store-free recorded-input admission descriptor; both land in distinct maps in `ProcessEngineRegistry`, with a unique `kind()` enforced. A `ProcessEngine` does not declare a durability tier. Replay is a mechanical property of the surrounding effect controller, and any end-to-end durability assertion belongs to the Host Application that composed the deployment.

2. **Session Plugin Options ride one materialization seam.** Root open and child create converge on one seam where plugin-keyed, serializable options reach the protocol plugin through one hook (the `configure_runtime_from_request` seam, generalized beyond child creation). The plugin owns apply-and-default — RLM defaults `final_answer_format` to Markdown for root sessions and RawFinalValue for children — and writes durable `protocol_turn_options`. Semantics are **apply-at-open**: options are re-resolved on every open, and durable state records the last applied value (this preserves the pre-existing behavior; it is not persist-at-create).

## Why

`RlmCoreBuilder::build` installed an out-of-band `runtime_host_installer` closure at the facade level to construct the Lashlang process engine, because engine construction needs the fully-built plugin host's extensions and the plugin contract had no post-registration host-level phase. The session-scoped registrar was the wrong scope for a runtime-host-scoped capability — but the host-level factory phase (`extension_contributions()`) already existed, so the contract extension mirrors it rather than inventing a new mechanism. Likewise, the facade's `apply_rlm_session_options` existed only because root and child session creation were asymmetric paths; the RLM protocol plugin already applied the same options for child sessions via `SessionCreateRequest.plugin_options`. Deleting the asymmetry makes the RLM case fall out of a general mechanism instead of relocating a special case.

## Consequences

- The facade's `runtime_host_installer` plumbing, `RlmCore`, `RlmCoreBuilder`, `RlmSessionBuilder`, and the `forward_core_builder_methods!` macro are deleted. There is exactly one builder type (`LashCoreBuilder`); `StandardCore::builder()`-style entry points become sugar functions returning a pre-seeded `LashCoreBuilder` (protocol factory + default runtime stack applied).
- `RlmProtocolPluginFactory` requires the backend its Lashlang artifacts live in at construction (`Backend::module_artifacts`, [ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)), making the previously build-time `MissingLashlangArtifactStore` error unrepresentable.
- The Lashlang compile APIs (`lashlang_compile_surface`, `compile_lashlang_module`) move to `lash-protocol-rlm` as operations over the factory and a plugin host; they had no production consumers outside facade tests.
- RLM per-session options are set through a facade sugar trait over the generic Session Plugin Options setter on `SessionBuilder`; every other plugin gets open-time options for free through the same seam.
- A durable rebuild (e.g. a Restate worker) reconstitutes process engines by installing the same plugins — consistent with ADR-0004's direction that plugins reconstitute their own capabilities.

## Considered Alternatives

- **Hook on `ProtocolDriverPlugin` for engine contribution.** Rejected: bakes in "engines come from protocols", which the plural `ProcessEngineRegistry` contradicts; a future non-protocol engine plugin would need the contract reopened.
- **Registrar capability for engine contribution.** Rejected: the registrar is session-scoped; process engines are runtime-host-scoped. The scope mismatch is exactly why the installer closure existed.
- **Contribution-envelope or intrinsic durability metadata.** Rejected: a component
  cannot verify the end-to-end deployment property it would be declaring. Process
  Engines contribute behavior; the Host Application owns deployment capability claims.
- **Keep the facade installer but make it public API.** Rejected: the facade stays the integration point and RLM stays special; does not meet the goal.
- **Resolve option defaults at read time instead of apply-at-open.** Rejected: less code, but silently re-answers a durable question on every read and changes observed behavior of existing sessions whenever a default changes.

## Amendment (FIG-4099, 2026-09-29): plugin options are creation config

Session Plugin Options are applied when the session is created, not on every
open. The seam is unchanged — the protocol plugin still resolves and defaults
them in `configure_runtime_on_materialize` — but a session that recorded its
protocol options keeps them exactly as recorded, whatever a later
materialization states: the RLM plugin applies stated options and fills its
defaults only for a session that has recorded none. A facade creation resolves
the builder's plugin options through the protocol plugin before the catalog
write, so they land in the initial config head with the catalog row. Every
creating path — `open()` of a new id, `create()`,
`open_with_state()`/`observe_with_state()` and the engine's own drive-open —
passes the creator's config to the catalog as
`SessionStoreCreateRequest::config`, and the store writes it as the session's
initial config head in the same transaction as the catalog row
(`SessionHeadMeta::created`). The request says which head it carries:
host-facing creation states `SessionCreationHead::Config`, so the head is on
disk before the session is first materialized, and it includes the protocol turn
options the session's protocol resolves at creation (the RLM session config
among them). A core runtime binding state it was handed states
`SessionCreationHead::CommittedByCreator`, and its first commit writes the head,
so only the row is written at admission. Admitting an id that already exists
writes no config either way. The facade forms that config in one place,
`SessionBuilder::creation_config`. A reopen reads the recorded head and writes
nothing: there is no reconciliation, no seed write and no report, and builder
config stated on a reopen is ignored. Only live policy follows an open (the
session binding, turn budget, autonomy, no-progress budget and charge safety),
and a builder provider that cannot serve the recorded pin is still refused typed
(`ProviderMismatch`) without a write. Every later change is the one durable
command, `update(SessionConfigPatch)`, which covers provider, model, prompt,
generation, attachment acceptance and plugin session config.

A change after creation reaches the protocol through the new
`ProtocolSessionPlugin::apply_session_config_patch` hook, fed by
`SessionConfigPatch::plugin_options`; a change the protocol will not make is
refused typed as `SessionError::SessionConfigRefused`, and a key no plugin
reads is refused as `PluginOptionsUnaccepted`.

This supersedes "Semantics are **apply-at-open**: options are re-resolved on
every open, and durable state records the last applied value" in the Decision,
and the "open-time options" wording in the Consequences.
