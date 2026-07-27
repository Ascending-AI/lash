# Protocol Capabilities Enter Through the Plugin Contract

## Status

accepted

## Decision

A protocol acquires runtime capabilities only through the uniform plugin contract — never through facade-level special wiring. Two contract extensions make this hold for RLM, the one protocol that needed more than a driver slot:

1. **Plugin-contributed Process Engines.** `PluginFactory` gains a host-level contribution method mirroring `extension_contributions()`: after the plugin host is built, core asks each factory for Process Engines, passing a read-only host context (built plugin extensions, trace context, host capabilities such as process-lifecycle availability). Engines land in the existing `ProcessEngineRegistry` with unique `kind()` enforced. A `ProcessEngine` does not declare a durability tier. Replay is a mechanical property of the surrounding effect controller, and any end-to-end durability assertion belongs to the Host Application that composed the deployment.

2. **Session Plugin Options ride one materialization seam.** Root open and child create converge on one seam where plugin-keyed, serializable options reach the protocol plugin through one hook (the `configure_runtime_from_request` seam, generalized beyond child creation). The plugin owns apply-and-default — RLM defaults `final_answer_format` to Markdown for root sessions and RawFinalValue for children — and writes durable `protocol_turn_options`. Semantics are **apply-at-open**: options are re-resolved on every open, and durable state records the last applied value (this preserves the pre-existing behavior; it is not persist-at-create).

## Why

`RlmCoreBuilder::build` installed an out-of-band `runtime_host_installer` closure at the facade level to construct the Lashlang process engine, because engine construction needs the fully-built plugin host's extensions and the plugin contract had no post-registration host-level phase. The session-scoped registrar was the wrong scope for a runtime-host-scoped capability — but the host-level factory phase (`extension_contributions()`) already existed, so the contract extension mirrors it rather than inventing a new mechanism. Likewise, the facade's `apply_rlm_session_options` existed only because root and child session creation were asymmetric paths; the RLM protocol plugin already applied the same options for child sessions via `SessionCreateRequest.plugin_options`. Deleting the asymmetry makes the RLM case fall out of a general mechanism instead of relocating a special case.

## Consequences

- The facade's `runtime_host_installer` plumbing, `RlmCore`, `RlmCoreBuilder`, `RlmSessionBuilder`, and the `forward_core_builder_methods!` macro are deleted. There is exactly one builder type (`LashCoreBuilder`); `StandardCore::builder()`-style entry points become sugar functions returning a pre-seeded `LashCoreBuilder` (protocol factory + default runtime stack applied).
- `RlmProtocolPluginFactory` requires the Lashlang artifact store at construction, making the previously build-time `MissingLashlangArtifactStore` error unrepresentable.
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
