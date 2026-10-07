# 0051. The facade is the host API; core exposes integrator seams

## Status

Accepted.

## Decision

The `lash` crate is the promised package API. Hosts and plugin authors compile
against the facade, including the integrator contracts they implement. ADR 0079
owns the package promise. The core crates expose the contracts required by
integrators and the cross-crate support needed to implement that facade.

The named integrator classes are:

### 1. Store implementors

Store implementors provide persistence, deployment-store, process, trigger,
attachment and artifact contracts and need their signature types.

### 2. Projection-provider implementors

Projection-provider implementors register a `ProjectionProvider` by type and
answer `read` and `read_range` for a `ResourceRef`
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §9). They
need the request, response and reference types.

### 3. Protocol, process-engine and tool implementors

These integrators implement `ProtocolSessionPlugin`, `ProtocolDriverPlugin`,
`CodeExecutorPlugin`, `ProcessEngine` or `ToolProvider`. A `ProcessEngine` is an
explicit state machine, `advance(state, event) -> (state, action)` (ADR 0132
§10). Every executable tool
registers through `ToolProvider`, whose required leaf route is
`execute(ToolCall<'_>) -> ToolAttemptOutcome`. A `ToolCall` holds an immutable
manifest and exposes its coherent name and ID. Completed and pending outcomes
are structurally distinct. ADR 0116 owns tool execution and declared work; a
provider describes a related session turn through a pending outcome and a
`DeclaredStart`, which the runtime launches.

### 4. Conformance-suite embedders

Conformance embedders exercise custom stores through
`lash-internal-conformance` directly. Conformance is an internal test package,
independent of the facade's host testing helpers.

### Signature closure

Integrator membership follows transitive signature closure. A type needed to
implement a public trait belongs to the contract even if no repository consumer
names it directly. The same rule applies to members:

- An implementor producing a value needs its constructors and builders.
- An implementor consuming an opaque value needs its accessors.
- A member used only by the runtime or facade implementation belongs in a
  private implementation or an explicit cross-crate support module.

Direction belongs to an integrator class, not to the type alone. Stores consume
`RuntimeCommit`; a conformance harness constructs it. Engines consume
their state and event values through their accessors. Direct-use scanning cannot
replace this reasoning, because an external implementor can depend on a member
with no in-repository caller.

`lash_core::facade_support` contains implementation seams the facade needs.
Host convenience can live on facade types or extension traits. Package version
constants do not arbitrate durable compatibility; trait contracts, data shapes
and the registered format contracts do.

## Plugin authoring

`lash::plugins` exposes plugin operations, hook arguments, state access,
protocol contracts and the types needed to implement them. Operations use
`PluginQuery`, `PluginCommand` and `PluginTask` with their matching contexts and
outcomes. Hosts invoke those operations through `lash::admin`. The facade's
single general prelude is `lash::prelude`.

`TurnContextTransform::transform` receives and returns `PreparedContext`.
`TurnContext` carries runtime correlation; per-send prompt overrides belong to
`RunSpec`. A plugin receives runtime-provided services rather than assembling
the runtime's authority.

Prompt sections follow [ADR 0133](0133-prompt-sections-are-keyed-trusted-and-placed-by-the-host.md): `lash::plugins` carries the section and wrapper
contracts, and `lash::prompt` the host's plan and the recorded snapshots.
Deleting `TurnContextTransform` is open work of ADR 0133 §9.

### Read-only handles

`DurableSession::read` returns settled `SessionReadView` data without opening a
runtime or acquiring execution ownership. SQLite's
`SqliteSessionStoreFactory::open_read_only` opens the catalog with `mode=ro`.
The read path does not mutate session, lease or graph state.

This is a database-state guarantee. SQLite can create WAL and shared-memory
sidecars when it materializes a wal-index. Read-only media requires those
sidecars to exist; failure is a backend error. `immutable=1` would be unsound
while another process holds a writer.

## Why

A public contract needs an owner and a consumer. Promising every core
implementation detail couples hosts to runtime internals. Hiding types that
implementors need makes a trait impossible to implement through the facade.
Signature closure keeps the supported contracts complete without turning
implementation access into a package promise.

Each facade API should have a compiled example or doctest. This is review
doctrine, not a universal coverage ledger. Current enforcement is facade-only
import scanning, the feature-plan check and compile-fail fixtures.

## Consequences

- Hosts and plugin authors use facade paths for their complete contracts.
- Core dependencies, including wire-conversion crates, use explicit lower-level
  seams where importing the facade would create a dependency cycle.
- A public core item needs an integrator contract or an implementation consumer;
  raw item counts are not a visibility target.
- Example import scanning rejects core, sans-io and internal-package imports in
  host examples. Compile-fail fixtures constrain forbidden host capabilities.

## Code evidence

- [Persistence contracts](../../crates/lash/src/lib.rs#L383) and
  [plugin and engine contracts](../../crates/lash/src/lib.rs#L534).
- [Core support module](../../crates/lash-core/src/lib.rs#L120).
- [Tool call and required execution route](../../crates/lash-core-execution/src/tool_provider.rs#L1337).
- [Internal conformance entry](../../crates/lash-conformance/src/lib.rs#L1).
- [Settled session read](../../crates/lash/src/durable_session.rs#L361).
- [Import scanning](../../scripts/check_facade_only_examples.py),
  [feature plan](../../scripts/check_feature_coverage.py), and
  [compile-fail fixtures](../../crates/lash/tests/ui.rs).
