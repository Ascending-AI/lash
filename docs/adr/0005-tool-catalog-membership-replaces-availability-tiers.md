# Tool Catalog membership defines availability

## Status

accepted

## Decision

The Tool Catalog is a flat set of callable tools. Membership is the availability fact. Plugins add and remove members; host suppression policies use non-membership. Model-request presentation and discovery are separate protocol and host choices.

Standard protocol presents all catalog members when discovery is absent. With discovery configured it presents inline members and, when enabled, batch dispatch. RLM presents tools according to its pinned cell or native channel under ADR 0083. Membership does not require every schema to appear inline in every request.

A host advertises a Deferred Package under a scoped provider identity. Its
members are resolved on first use at runtime, independently for each Operation
Use. The runtime atomically records the validated positive or negative resolution
before dispatch; a valid resolution stays pinned even when this call's
arguments then fail to fit. Later calls, parks, replay and carry
reuse that immutable answer; a new execution resolves afresh. Resolution never
changes the session Tool Catalog. An unresolved grant records the provider's
owner, key and configuration, and claim requirements include its discovery
debt until the answer is committed. A resolved target records its own route
and requirements, so a cold continuation does not need the retired resolver.

## Why and alternatives

An availability ladder mixes callability, prompt presentation and discovery into one ordering even though they vary independently. A resident-but-searchable tier changes request budgeting without changing the runtime's ability to call the tool. Both are rejected. Resolver enumeration and preview methods are rejected because ranking, discovery and previews belong to host tools and the host's recorded `PromptPlan`. Protocols contribute keyed prompt sections; the host plan orders and places them (ADR 0133).

## Consequences

Hosts own discovery and Deferred Package providers. `examples/agent-workbench` supplies a host implementation. Standard discovery reaches catalog members through its admitted dispatch paths; first-use deferred resolution belongs to the execution.

[Standard presentation](../../crates/lash-protocol-standard/src/lib.rs),
[scoped providers](../../crates/lash-protocol-rlm/src/deferred.rs) and
[recorded resolution](../../crates/lash-durable/src/resolutions.rs) implement
the decision.
