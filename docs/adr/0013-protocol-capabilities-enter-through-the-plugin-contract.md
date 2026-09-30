# Protocol capabilities enter through the plugin contract

## Status

accepted

## Decision

Protocols acquire runtime capabilities through the uniform plugin contract. After building the plugin host, core asks factories for `process_engine_contributions`, passing extensions, trace context and process-lifecycle availability. Registrations pair an execution engine with a store-free recorded-input admission descriptor. The engine registry enforces unique kinds and stores execution and admission separately. Deployment durability claims belong to the host that composes the engine and storage.

Session Plugin Options are creation configuration. The protocol plugin resolves options and defaults when it creates the session's initial config. RLM defaults root final answers to Markdown and child answers to RawFinalValue. Materialization preserves recorded protocol options; it fills defaults only when none are recorded.

Host-facing creation carries `SessionCreationHead::Config`, so the catalog row and initial config head commit together. A core creator carrying its own runtime state uses `CommittedByCreator` and writes the head with its first commit. Admission of an existing id does not write creation config. Reopening reads recorded state and ignores newly stated builder configuration; live binding and budgets remain open-time host policy. An incompatible provider pin is refused as `ProviderMismatch` without a config write.

Later changes use `update(SessionConfigPatch)`. Plugin options reach `ProtocolSessionPlugin::apply_session_config_patch`; refused changes return `SessionConfigRefused`, and unread plugin keys return `PluginOptionsUnaccepted`.

## Why and alternatives

Facade-specific engine installers bypass the contract and are rejected. A session registrar is the wrong owner for host-scoped engines. Protocol-only engine contribution hooks are rejected because ordinary plugins may contribute engines too. Read-time or reopen-time default resolution is rejected because it changes durable behavior when host defaults change.

## Consequences

One `LashCoreBuilder` assembles protocol and common plugins. RLM's factory requires its backend artifact port and contributes the process engine; its compile operations live with the protocol. Durable workers reconstruct the same plugin capabilities from captured options.

[Engine contributions](../../crates/lash-core-execution/src/plugin/runtime_impl.rs), [RLM materialization and patches](../../crates/lash-protocol-rlm/src/plugin/protocol_session.rs), [session creation](../../crates/lash/src/session.rs) and [creation head types](../../crates/lash-core-store/src/session_identity.rs) implement the decision.
