# Process environments carry ordered process component configs

## Decision

An engine process runs under the complete immutable `ProcessEnv` its starter
states. It carries model data, attachment rules, limits, Restricted tool
access, capability refs and ordered process component configs. Each component
declares one format for both its config and step-state namespace. A component
decodes its own config to reconstruct its providers, catalog contributions,
hooks, presentation and named-session read grants. These are process
components from a separate registry; session plugin options are a session's
configuration. Component order is composition order and part of the content
address.

A product's process execution data belongs in its component's config, as
`workflow_cost` does. The host chooses that immutable data at start; lash
neither infers it from session state nor fills it from deployment defaults.
Mutable product metadata stays in the host's own store. Hosts own
authorization and revocation policy.

Publication admits the environment against the installed process components.
A process builds only the components its environment names. Submission admits
immutable process identity and recorded input; worker execution checks the
linked requirements before compilation or effects.

Component state publications carry this format, and claim requirements use
`ObligationToken::for_component` for every component's recorded state/config
format. A node serves each format its installed component can read.

On reopen, before any component binds or step runs, the owner reconstructs
the complete committed namespace view. A readable older view converts through
the same namespace machinery as session plugins. The owner records the whole
state and config postimage with the new requirements in one fenced driver
cut. Original publication receipts remain intact; later reopens install the
recorded postimage and replay subsequent publications without reconverting.
The immutable starting environment retains its identity; the cut changes its
component representations, without selecting new configuration or authority.

## Consequences

Restart and adoption reconstruct the recorded component configuration without
re-selecting authority. Infrastructure failures remain runtime faults.
Unservable components and formats keep the process unclaimed or parked,
while cancellation can end it without building its components
([ADR 0110](0110-the-engine-owns-process-recovery.md),
[ADR 0132](0132-durability-is-state-first-over-the-lash-store.md)).

[Process environments](../../crates/lash-core-store/src/process_env.rs),
[components](../../crates/lash-core-execution/src/plugin/process_components.rs)
and [publication](../../crates/lash-core-execution/src/runtime/process/env_hold.rs)
implement these boundaries.
