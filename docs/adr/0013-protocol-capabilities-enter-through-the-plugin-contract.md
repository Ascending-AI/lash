# Protocol capabilities enter through the plugin contract

## Status

accepted

## Decision

Protocols acquire runtime capabilities through the uniform plugin contract. After building the plugin host, core asks factories for `process_engine_contributions`, passing extensions, trace context and process-lifecycle availability. Registrations pair an execution engine with a store-free recorded-input admission descriptor. The engine registry enforces unique kinds and stores execution and admission separately. Deployment durability claims belong to the host that composes the engine and storage.

Session Plugin Options are creation configuration. At creation every installed plugin factory, the protocol's included, creates its own namespace and defaults through the `ConfigOwner` it registered with `PluginFactory::register_config`. The session records the results as its `PluginConfig`, keyed by plugin id, with its initial config head (FIG-4379). RLM defaults root final answers to Markdown and child answers to RawFinalValue. The protocol turn options are a view of the protocol's recorded namespace. Every open supplies the recorded values unchanged and resolves nothing again.

Host-facing creation carries `SessionCreationHead::Config`, so the catalog row and initial config head commit together. A core creator carrying its own runtime state uses `CommittedByCreator` and writes the head with its first commit. Admission of an existing id does not write creation config. Reopening reads recorded state and ignores newly stated builder configuration; live binding and budgets remain open-time host policy. An incompatible provider pin is refused as `ProviderMismatch` without a config write.

Later changes are typed config commands the namespace's owner registered, applied in one revision-checked transaction ([ADR 0126](0126-session-config-changes-are-typed-owner-commands.md)). A creation namespace no installed plugin owns is refused with `UnknownPluginConfigOwner`; a command no installed plugin registers is refused at submission.

## Why and alternatives

Facade-specific engine installers bypass the contract and are rejected. A session registrar is the wrong owner for host-scoped engines. Protocol-only engine contribution hooks are rejected because ordinary plugins may contribute engines too. Read-time or reopen-time default resolution is rejected because it changes durable behavior when host defaults change.

## Consequences

One `LashCoreBuilder` assembles protocol and common plugins. RLM's factory requires its backend artifact port and contributes the process engine; its compile operations live with the protocol. Durable workers reconstruct the same plugin capabilities from captured options.

[Engine contributions](../../crates/lash-core-execution/src/plugin/runtime_impl.rs), [RLM materialization and patches](../../crates/lash-protocol-rlm/src/plugin/protocol_session.rs), [session creation](../../crates/lash/src/session.rs) and [creation head types](../../crates/lash-core-store/src/session_identity.rs) implement the decision.
