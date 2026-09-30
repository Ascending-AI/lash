# Process environments carry plugin options, not product metadata

## Decision

Process execution environments are typed, closed specifications containing plugin options, session policy and recorded rendering configuration. Plugins decode their own options to reconstruct providers, Tool Catalog entries, grants and execution bindings. Product-specific tool state belongs in immutable host-owned snapshots referenced by those options. Hosts own authorization and revocation policy.

Submission admits immutable process identity and the closed recorded-input shape. Preparation checks artifact process and Host Requirements references while omitting live host-environment validation. The worker reconstructs the plugin environment and checks it against the linked Host Requirements before compilation or effects.

## Consequences

Permanent reconstruction refusals produce logical process failure or the typed resume refusal appropriate to an incompatible stored generation. Infrastructure failures remain worker/runtime failures and follow engine recovery. Mutable product metadata in the environment is rejected because replay must reconstruct the captured contract rather than the creator's current state.

[Environment specifications](../../crates/lash-core-store/src/process_identity.rs), [preparation and recorded-input admission](../../crates/lash-lashlang-runtime/src/lib.rs) and [worker validation](../../crates/lash-lashlang-runtime/src/process.rs) implement the boundaries.
