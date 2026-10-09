# 0095: Processes are values, process controls are tools, one handle kind

Status: Accepted

## Context

Tools need to receive and return executable definitions without language
special forms or an untyped definition object. Durable waits and tool calls
also need one handle codec and one recorded settlement order.

## Decision

### A process definition is a value

A definition is a value of type `Process<(params), out>`. The process-control
catalog supplies `processes.create`, `get`, `start`, `await`,
`cancel`, and `list`. Tools describe process values through their contracts.
TypeScript async arrows are the source process literals under
[ADR 0096](0096-typescript-is-the-sole-rlm-dialect.md).

Hosts supply event and registration tools through ordinary tool contracts.
[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns their routing, input checks and keyed delivery.

Process handles retain identity in nested containers through suspension and
snapshots. Process lifetime and identity follow
[ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md) and
[ADR 0108](0108-a-process-lives-until-a-scope-its-start-could-reach.md).

Evidence: `crates/lash-vm-runtime/src/process_create_tool.rs`,
`crates/lash-vm/src/runtime/vm/continuation.rs`, and
`crates/lash-vm/src/linker/`.

### Contracts say `Process` through one tagged keyword

JSON Schema uses one tagged `x-lash` extension with
`{ kind: "process", signature }`, `{ kind: "process_unknown" }`, and
`{ kind: "handle", payload }` for a process handle. Malformed extensions are
refused. A signature on a value
is a claim; engine resolution supplies authority and refuses mismatches before
registration, under
[ADR 0090](0090-named-process-signatures-are-authoritative.md).

Evidence: `crates/lash-vm/src/json_schema.rs`,
`crates/lash-vm-runtime/src/process/schema.rs`,
`crates/lash-core-execution/src/runtime/process/definition.rs`, and
`crates/lash-vm/src/linker/catalog.rs`.

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
nothing. Frames, process records, starts, executions, and
host pins hold the artifact closure under
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).
Both a definition value and its tagged id can retain the closure in a frame;
`continue_as` carries only the values its seed supplies.

Evidence: `crates/lash-core-execution/src/runtime/process/definition.rs`,
`crates/lash-sansio/src/definition_id.rs`,
`crates/lash-core-execution/src/runtime/process/definition_store.rs`, and
`crates/lash-vm-runtime/src/process_create_tool.rs`.
The independent golden vectors and laws live in
`crates/lash-core-execution/src/runtime/process/definition_tests.rs`.

### No new intent-identity mechanism

The recorded tool call and attempt supply the start identity. A stable
`StartKey` identifies a start, while the registrar mints the process id once.
A repeated start returns that process. VM code-call identity is the admitted
operation identity: the issue ordinal is assigned when the operation is
admitted and commits with the VM snapshot
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §8);
structural node id and occurrence are telemetry.

Evidence: `crates/lash-core-execution/src/runtime/process/model/start_request.rs`,
`crates/lash-core-store/src/process_identity.rs`, and
`crates/lash-vm-runtime/src/host_identity.rs`.

### Literals lift syntactically, and are accepted type-directed

The front end discovers inline async arrows syntactically. The linker accepts
one only where the expected type contains `Process`, then hoists its declaration
using canonical body and AST path. Other slots receive a typed refusal.
Immutable, durably representable captured locals become hidden parameters.
Host callbacks and approvals are deferring tool calls with checked schemas
under ADR 0137. No call-site marker owns literal lifting.

Evidence: `crates/lash-vm/src/linker/process_literal.rs`, and `crates/lash-typescript/src/lower`.

### One handle kind, and await is a Durable Wait

The shared handle record is `{ __handle__: "lash", id }`. A process handle id
is `p.<minted process id>`; the codec distinguishes that target from a tool
request. Pending requests use handle ids.

`processes.await` returns Deferred on a process-terminal source, a
`process_terminal` wait row (ADR 0132 §6 and §11). The logical Run retains its
descriptor through snapshot and resume and takes a rank only when the resolved
wait becomes a final decision. There is no local body waiting on a long attach
handler. A raw process handle in an aggregate must be replaced
by the explicit await call.

A losing wait stays admitted while the logical Run lives. Closing releases that
subscription under `Ignore` without cancelling the observed process. The
process's independent lifetime still applies. `Resolved(ref)` carries the
canonical `ProcessAwaitOutput`, including process failure or cancellation;
`Cancelled` is cancellation of the observing wait. No source timeout exists.

Evidence: `crates/lash-sansio/src/handle.rs`,
`crates/lash-vm/src/runtime/vm/pending_tools.rs`, and
`crates/lash-core-execution/src/runtime/effect/executor/process_local.rs`.
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md)
owns Run settlement, and
[ADR 0016](0016-process-waits-live-on-the-work-driver-seam.md) owns Durable Wait.

### The workflow graph sees calls, not a start effect

A process start projects as a call; a literal projects as a process container.
The code-graph-code lens follows
[ADR 0037](0037-lash-vm-workflows-use-a-code-graph-code-lens.md).
Evidence: `crates/lash-vm/src/workflow_graph/projection.rs`.

## Alternatives considered

A marker at the literal site duplicates the expected type the linker already
checks. Language aliases for controls duplicate catalog teaching and lowering.
An atomic batch requiring a process terminal to settle inside one resource
operation cannot represent waits lasting days. Independently durable group
children use durable wait rows and one recorded settlement order.

## Consequences

- Third-party tools can declare process-valued contracts.
- Process controls evolve through tool contracts and plugins.
- Engine authority protects registration from forged signature claims.
- Definition names and versions are host data; content and retention are Lash data.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
