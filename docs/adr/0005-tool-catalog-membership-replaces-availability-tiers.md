# Tool Catalog membership defines availability

## Status

accepted

## Decision

The Tool Catalog is a flat set of callable tools. Membership is the availability fact. Plugins add and remove members; host suppression policies use non-membership. Model-request presentation and discovery are separate protocol and host choices.

Standard protocol presents all catalog members when discovery is absent. With discovery configured it presents inline members and, when enabled, batch dispatch. RLM presents tools according to its pinned cell or native channel under ADR 0083. Membership does not require every schema to appear inline in every request.

RLM's host-provided `DeferredToolResolver` resolves missing call paths during linking, in the context of the execution that links: its owner, its logical Run, and the capability refs that Run's spec recorded. The runtime gathers unresolved paths, excludes recorded outcomes, resolves one batch, folds grants into the link environment, and links. Missing answers become `NotAvailable`. Outcomes, including negative answers and Tool Execution Bindings, are frozen under the cell's admitted execution and commit with its VM snapshot ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §8). Resume reinstalls recorded routing through the host hook and reuses the outcomes; a different cell starts a fresh record. Resolution does not mutate the session catalog.

A deferred grant's source names its owning plugin. Providers registered with `LashCoreBuilder::tools` belong to `lash::tools::PLUGIN_TOOL_SOURCE_ID` (`embed_tools`); the builder's plugin declaration and the tool registry use that same identity. Grants for other plugins name those plugins' ids. Grant execution and restore resolve that exact source, without searching other plugins for a matching tool.

## Why and alternatives

An availability ladder mixes callability, prompt presentation and discovery into one ordering even though they vary independently. A resident-but-searchable tier changes request budgeting without changing the runtime's ability to call the tool. Both are rejected. Resolver enumeration and preview methods are rejected because ranking, discovery and previews belong to host tools and the host's recorded `PromptPlan`. Protocols contribute keyed prompt sections; the host plan orders and places them (ADR 0133).

## Consequences

Hosts own discovery and grant storage. `examples/agent-workbench` supplies a host implementation. Standard discovery reaches catalog members through its admitted dispatch paths; link-time deferred resolution belongs to RLM.

[Standard presentation](../../crates/lash-protocol-standard/src/lib.rs), [link records](../../crates/lash-vm-runtime/src/deferred.rs) and [recorded resolution](../../crates/lash-vm-runtime/src/deferred/journal.rs) implement the decision.
