# 0095: Processes are values, process controls are tools, one handle kind

Status: Accepted

## Context

Tools need to receive and return executable definitions without language
special forms or an untyped definition object. Durable waits and tool calls
also need one handle codec and one recorded settlement order.

## Decision

### A process definition is a value

A definition is a value of type `Process<(params), out>`. The process-control
catalog supplies `processes.create`, `get`, `start`, `await`, `emit`, `signal`,
`cancel`, and `list`. Tools describe process values through their contracts.
TypeScript async arrows are the source process literals under
[ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md).

`triggers.register` is a declaring leaf tool. It validates a registration and
declares `ToolIntent::RegisterTrigger`; realization installs the subscription.
Other trigger administration operations use `TriggerHostOperation`.

Process handles retain identity in nested containers through suspension,
snapshots, and replay. Process lifetime and identity follow
[ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md) and
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md).

Evidence: `crates/lash-lashlang-runtime/src/process_create_tool.rs:31`,
`crates/lash-lashlang-runtime/src/trigger_tools.rs:1`,
`crates/lashlang/src/runtime/vm/continuation.rs:140`, and
`crates/lashlang/src/trigger.rs`.

### Contracts say `Process` through one tagged keyword

JSON Schema uses one tagged `x-lash` extension with
`{ kind: "process", signature }`, `{ kind: "process_unknown" }`, and
`{ kind: "handle", payload }` for a trigger handle. Malformed extensions are
refused. A signature on a value
is a claim; engine resolution supplies authority and refuses mismatches before
registration, under
[ADR 0090](0090-named-process-signatures-are-authoritative.md).

Evidence: `crates/lashlang/src/json_schema.rs:24`,
`crates/lash-lashlang-runtime/src/process/schema.rs`,
`crates/lash-core-execution/src/runtime/process/definition.rs:417`, and
`crates/lashlang/src/linker/catalog.rs`.

### Definitions are immutable values

`ProcessDefinitionDraft { engine_kind, value, artifacts }` names the owning
engine, canonical engine value, and artifact manifest. The id is
`lash.definition:sha256:<64 lowercase hexadecimal digits>`, a SHA-256 digest
under `lash.process-definition-id`, family version 1.

The framed preimage encodes engine kind, `identity_json::payload_leaf(value)`,
and artifact leaves sorted by their encoded bytes and deduplicated. Each leaf
has a permanent store tag and reference, plus engine kind for an engine-owned
artifact. Lengths and counts are big-endian. Canonical JSON normalizes object
key order and signed zero while distinguishing `1` from `1.0`. The signature
claim is excluded; the engine derives it from the descriptor. The descriptor's
manifest must match the engine's resolved requirements.

In JSON values the id is `{"$lash_definition_id": "<spelling>"}`. SQL columns
and logs use its spelling. `lash-sansio` owns the codec. A definition value adds
its signature claim. An unknown claim asserts nothing and adopts the derived
signature; a known mismatch is refused.

```typescript
type DefinitionId = { $lash_definition_id: string };
type ProcessSignature = { signature: "unknown" } | { signature: "known"; encoding: unknown };
type Definition = { id: DefinitionId; signature: ProcessSignature };
type ProcessHandle = { __handle__: "lash"; id: string };
type Target = { definition: Definition } | { definition_id: DefinitionId };
processes.create({ source: string, dialect: "typescript" }): Promise<Definition>;
processes.start(Target & { args?: Record<string, unknown>; label?: string }): Promise<ProcessHandle>;
processes.get({ definition_id: DefinitionId }): Promise<Definition>;
```

Exactly one target is required, and args default to `{}`. The descriptor supplies
the engine. Create compiles in the attempt and declares `PublishDefinition`;
realization publishes behind the committed attempt. Lifted literals and module
exports use immutable definitions too. Missing definitions, corruption,
unavailable engines, and signature mismatches are typed failures.

Lash has no named-definition registry, definition revisions, compare-and-swap,
or replacement. Lash supplies no model operation to replace, delete, or list a
definition catalog. Hosts own names and versions. Publication and pinning use
`HostArtifactPin`; reads acquire no lasting pin. A content id alone retains
nothing. Frames, process records, subscription revisions, starts, journals, and
host pins hold the artifact closure under
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).
Both a definition value and its tagged id can retain the closure in a frame;
`continue_as` carries only the values its seed supplies.

Evidence: `crates/lash-core-execution/src/runtime/process/definition.rs:89`,
`:148`, `:189`, `:274`, `:417`,
`crates/lash-sansio/src/definition_id.rs`,
`crates/lash-core-execution/src/runtime/process/definition_store.rs:151`, and
`crates/lash-lashlang-runtime/src/process_create_tool.rs:1`.
The independent golden vectors and laws live in
`crates/lash-core-execution/src/runtime/process/definition_tests.rs:145`,
`:259`, `:329`, `:408`.

### No new intent-identity mechanism

The recorded tool call and attempt supply the start identity. A replay-stable
`StartKey` identifies a start, while the registrar mints the process id once.
Replaying the start returns that process. VM code-call identity is positional:
issue ordinal names a command; structural node id and occurrence are telemetry.

Evidence: `crates/lash-core-execution/src/runtime/process/model/start_request.rs`,
`crates/lash-core-store/src/process_identity.rs`, and
`crates/lash-lashlang-runtime/src/host_identity.rs:1`.

### Literals lift syntactically, and are accepted type-directed

The front end discovers inline async arrows syntactically. The linker accepts
one only where the expected type contains `Process`, then hoists its declaration
using canonical body and AST path. Other slots receive a typed refusal.
Immutable, durably representable captured locals become hidden parameters.
The linker infers signals from `waitSignal` sites and refuses disagreeing
payload types. No call-site marker or trigger-receiver special case owns lifting.

Evidence: `crates/lashlang/src/linker/process_literal.rs:1`, `:21`, `:56`,
`:112`, and `crates/lash-typescript/src/lower`.

### One handle kind, and await is a Durable Wait

The shared handle record is `{ __handle__: "lash", id }`. A process handle id
is `p.<minted process id>`; the codec distinguishes that target from a tool
request. Pending requests use handle ids.

`processes.await` parks on the Durable Wait seam. In an effect group it is a
resumable child, retained across segment boundaries, and takes its rank when
completion arrives. There is no running attempt body to join at close.
A raw process handle at an aggregate element must be replaced by the
`processes.await` call so the wait participates in group settlement.

Selection leaves a losing wait admitted while the opener lives. Opener close
releases it without cancelling the process. The process's own lifetime still
applies. Process success, failure, or cancellation travels through
`Resolution::Ok` as a `ProcessAwaitOutput`; `Resolution::Cancelled` describes
wait cancellation and becomes `process_await_cancelled`.

Evidence: `crates/lash-sansio/src/handle.rs:46`, `:85`, `:142`,
`crates/lashlang/src/runtime/vm/pending_tools.rs:176`,
`crates/lash-restate/src/process/mod.rs:208`, `:241`, and
`crates/lash-core-execution/src/runtime/effect/executor/process_local.rs:1`.
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md)
owns group settlement, and
[ADR 0016](0016-process-waits-live-on-the-work-driver-seam.md) owns Durable Wait.

### The workflow graph sees calls, not a start effect

A process start projects as a call; a literal projects as a process container.
The code-graph-code lens follows
[ADR 0037](0037-lashlang-workflows-use-a-code-graph-code-lens.md).
Evidence: `crates/lashlang/src/workflow_graph/projection.rs:390`, `:660`.

## Alternatives considered

A marker at the literal site duplicates the expected type the linker already
checks. Language aliases for controls duplicate catalog teaching and lowering.
An atomic batch requiring a process terminal to settle inside one resource
operation cannot represent waits lasting days. Independently durable group
children use the Durable Wait protocol and one recorded settlement order.

## Consequences

- Third-party tools can declare process-valued contracts.
- Process controls evolve through tool contracts and plugins.
- Engine authority protects registration from forged signature claims.
- Definition names and versions are host data; content and retention are Lash data.
