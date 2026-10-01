# Lashlang execution bounds span durable process lifetimes

## Status

Accepted.

## Context

TypeScript lowers into the Lashlang IR and VM. Execution can be a foreground
RLM cell or a durable process that parks at effects and resumes from persisted
continuations. Hosts need bounds on instruction work and logical heap size,
and those bounds need a defined lifetime across segment handovers.

## Decision

RLM configuration explicitly supplies two independent bounds:

- `instruction_limit: InstructionBound` covers VM instructions and charged
  intrinsic work.
- `memory_limit: MemoryBound` covers logical heap bytes.

`InstructionBound::instructions(n)` and `MemoryBound::logical_bytes(n)` or
`MemoryBound::mebibytes(n)` construct nonzero bounds. Each type also has an
explicit `unbounded()` opt-out. The distinct protocol types prevent supplying
an instruction count as the memory argument.

`RlmProtocolPluginConfig::builder()` exposes `build()` only after the host
supplies both bounds and its channel. Serialized configuration requires both
bound fields. Their wire values are `{"bounded": 1000000}` or `"unbounded"`.
The protocol's `ExecutionBounds::new` also requires both axes. The underlying
VM host contract separately supplies a default logical-memory ceiling of
512 MiB for a host that does not override `execution_bounds`.

The configured bounds are a deployment's creation default. A session records
the bounds it was created under in its RLM namespace (`RlmRecordedBehaviour`,
FIG-4398). Its cells, and every process it starts, run under the recorded
bounds: a process reads them from the plugin configuration it captured with
its execution environment. A deployment configured with other bounds that
opens, redrives or resumes the session does not change them.

A process whose captured plugin configuration has no RLM namespace, such as
one a host starts under an environment it published itself, records the
creating deployment's behaviour with its row instead (FIG-4527). The engine
states it once, in the start's recorded registration step
(`ProcessEngine::creation_config`, `ProcessRecord::engine_config`), and
every run reads it back. A start that finds a row retained under its key is
returned that row with what its own creation recorded. A process that
recorded its behaviour in neither place is refused; no run reads the bounds
of the deployment it runs on.

A hand-built `LashlangProcessEngine` records its constructor surface and
bounds with `engine_config` at creation too. Constructor setters supply
creation defaults only. Runs use the registration reconstructed from the
record, including after a remote round trip; `RemoteProcessRecord` carries
the engine configuration unchanged. Missing recorded settings are a typed
`PluginError::MissingRecordedProcessConfig` refusal (FIG-4558).

### Heap accounting

The VM meters logical bytes under its registered heap-size schedule rather
than allocator or RSS measurements. In the current production schedule, an
object header costs 16 bytes, a value slot costs 64 bytes plus its scalar
payload, and a record field additionally costs 8 bytes plus its UTF-8 key.
A reference has an 8-byte payload. Object kinds account for their own fields,
including closure metadata and exotic state.

Allocation precharges objects and collection subtracts swept objects. The
non-moving mark-sweep collector runs every 1,024 allocations and at boundaries
that need an exact live set, including parks and snapshot capture. Stress
collection uses rooted allocation scopes.

The memory limit includes allocated bytes awaiting collection, so collection
timing can affect when a memory bound fires. A park can remove unreachable
objects sooner than straight-through execution. This does not reset reachable
heap accounting or the instruction meter.

### Lifetime and enforcement

Foreground instruction meters apply per executed cell. A durable process
persists its execution counters and heap accounting across segment handovers;
resuming does not grant a fresh process instruction budget.

The VM checks bounds on resumed execution, after intrinsic dispatch, at effect
boundaries, cooperative yields and terminal exits. Proportional intrinsic work
charges the instruction meter, with bounded dispatch/check overshoot. A
separate maximum frame depth limits call frames. No execution bound reads a
wall-clock deadline.

Exhaustion is a typed terminal failure. The durable engine exposes
`process_execution_bound_exhausted`. Confidence assertions make exhausted
bounds loud in the relevant harness paths.

Format writers and readers use the registered versions and fleet read windows.
`lash::formats` exposes the current format manifest. ADR 0115 governs upgrades
and drain boundaries; the pre-1.0 freeze changes shapes in place without bumps
or upcasters.

## Consequences

- Hosts explicitly choose the RLM instruction and memory policy.
- Durable handovers preserve cumulative instruction accounting.
- Logical memory is a reproducible accounting schedule, not a promise about
  physical resident memory.
- Bound failures are terminal rather than requests to retry unchanged work.

## Code evidence

- [Protocol bounds](../../crates/lash-protocol-rlm/src/plugin/config_types.rs#L47) and
  [required configuration](../../crates/lash-protocol-rlm/src/plugin/config.rs#L63).
- [VM host bounds and default](../../crates/lashlang/src/runtime/host.rs#L387).
- [Heap charges](../../crates/lashlang/src/runtime/heap/object.rs#L41) and
  [schedule and collection interval](../../crates/lashlang/src/runtime/heap.rs#L50).
- [Enforcement](../../crates/lashlang/src/runtime/vm/control.rs#L545).
- [Durable failure code](../../crates/lash-lashlang-runtime/src/error.rs#L334).
