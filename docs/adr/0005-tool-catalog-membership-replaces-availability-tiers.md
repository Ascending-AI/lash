# Tool Catalog membership defines availability

## Status

accepted

## Decision

The Tool Catalog is a flat set of callable tools. Membership is the availability fact. Plugins add and remove members; host suppression policies use non-membership. Model-request presentation and discovery are separate protocol and host choices.

Standard protocol presents all catalog members when discovery is absent. With discovery configured it presents inline members and, when enabled, batch dispatch. RLM presents tools according to its pinned cell or native channel under ADR 0083. Membership does not require every schema to appear inline in every request.

RLM's host-provided `DeferredToolResolver` resolves missing call paths during linking. The runtime gathers unresolved paths, excludes recorded outcomes, resolves one batch, folds grants into the link environment, and links. Missing answers become `NotAvailable`. Outcomes, including negative answers and Tool Execution Bindings, are frozen under the stable `ExecCode` address. Replay reinstalls recorded routing through the host hook and reuses the outcomes; a different code effect starts a fresh record. Resolution does not mutate the session catalog.

## Why and alternatives

An availability ladder mixes callability, prompt presentation and discovery into one ordering even though they vary independently. A resident-but-searchable tier changes request budgeting without changing the runtime's ability to call the tool. Both are rejected. Resolver enumeration and preview methods are rejected because ranking, discovery and previews belong to host tools and prompt contributions.

## Consequences

Hosts own discovery and grant storage. `examples/agent-workbench` supplies a host implementation. Standard discovery reaches catalog members through its admitted dispatch paths; link-time deferred resolution belongs to RLM.

[Standard presentation](../../crates/lash-protocol-standard/src/lib.rs), [link records](../../crates/lash-lashlang-runtime/src/deferred.rs) and [journaled resolution](../../crates/lash-lashlang-runtime/src/deferred/journal.rs) implement the decision.
