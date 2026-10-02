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
FIG-4398), and its cells use those bounds. At process creation, the RLM
recorder maps that captured behaviour and the creation-time resource catalog
into `LashlangRecordedSettings`. Without a captured RLM namespace, it maps
the creating deployment's defaults instead (FIG-4527).

Both RLM-contributed and hand-built `LashlangProcessEngine` instances record
the same engine-owned shape in `ProcessRecord::engine_config` through
`ProcessEngine::creation_config` (FIG-4664). It contains abilities, language
features, resources and execution bounds. Constructor setters supply
creation defaults only. A start that finds a retained row returns that row's
settings, and every run reads only those settings. A deployment opening,
redriving or resuming a process cannot override them through its RLM
namespace or live extensions. Available process wiring can restrict a
recorded ability but cannot enable a recorded-disabled ability.

Runs use the registration reconstructed from the record, including after a
remote round trip; `RemoteProcessRecord` carries the engine configuration
unchanged. Missing settings are a typed
`PluginError::MissingRecordedProcessConfig` refusal (FIG-4558), and malformed
settings are `PluginError::StoredDataCorrupt`. Both remain typed terminal
causes across the host boundary. Neither path falls back to deployment
defaults or another recorded home.

### Heap accounting

The VM meters logical bytes from heap objects rather
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
