# Lashlang execution bounds span durable process lifetimes

## Status

Accepted.

## Context

Lashlang can execute in a foreground RLM block or as a durable process whose VM
parks at effects and resumes from persisted continuations. Hosts need protection
from both runaway instruction streams and expensive individual builtins. They
also need to know whether a configured limit applies to one foreground block,
one durable segment, or the whole logical process, and whether time spent
waiting on tools consumes the deadline.

A wall-clock deadline is necessarily less replay-stable than an instruction
budget. In particular, an uncommitted segment tail is measured again after a
crash. A redrive can therefore exhaust the active-time deadline even when the
original attempt did not. Crash loops do not accumulate uncommitted tail time,
because only committed continuation meters survive.

## Decision

Every RLM configuration must explicitly choose three independent bounds:

- `instruction_budget: ExecutionBound<NonZeroU64>` limits VM instructions plus
  collection work charged by builtins;
- `deadline: ExecutionBound<Duration>` limits active VM execution time; and
- `memory_limit: ExecutionBound<NonZeroU64>` limits live logical heap bytes.

The engine's own `ExecutionBounds::new` takes all three as well: a host that has
not decided how much logical memory an execution may hold has not finished
configuring it, and a silent default would be a bound nobody chose.

Hosts select a finite bound or `Unbounded` for each field. Rust callers should
use `ExecutionBound::instructions(n)`, `ExecutionBound::millis(n)`, or
`ExecutionBound::secs(n)` as appropriate; the byte-valued memory limit uses the
same nonzero integer bound representation as instructions. Serialized RLM
configuration must contain all three fields and has no implicit memory limit.
It uses
`{"bounded": 1000000}` for instructions and milliseconds, or the string
`"unbounded"`; duration serialization never exposes Rust's internal
seconds/nanoseconds representation.

Memory is metered by Lashlang heap size schedule v1, never by allocator or RSS
measurements. A heap object costs a 16-byte header; each value slot costs 16
bytes plus its deterministic scalar payload; records additionally cost 8 bytes
plus UTF-8 key bytes per field. References cost 8 payload bytes. Allocation
charges the complete object and mark-sweep collection subtracts swept objects.
The non-moving collector runs every 1,024 allocations, based only on the
monotonic allocation counter, and additionally wherever a boundary needs the
live set exactly: at a park, when a snapshot is captured, and when a batch of
global patches commits. Test hosts can collect after every allocation to prove
that collection timing does not change a program's result; every instruction
runs inside an allocation scope so that stress mode never collects against an
empty root set.

The memory limit bounds live plus not-yet-collected bytes, so it is the one
place where collection timing is observable: a run that parks collects earlier
than a run that does not, and can therefore survive a point at which the
straight-through run would have exhausted the bound. The relation is one-way —
parking never brings exhaustion forward — and results, instruction meters and
reachable heap accounting are unaffected.

Foreground meters apply per executed Lashlang block. Durable-process meters are
cumulative over the entire logical process lifetime and persist across every
segment handover. This asymmetry is intentional and must inform the values a
host chooses.

The deadline measures active VM time only. Awaited host and tool latency is
excluded. Bounds are checked on resume, after each intrinsic dispatch, before
and after effect boundaries, at cooperative yields, and on every VM exit.
Collection builtins charge at least their input size and sorting charges
`n log n`, so enforcement has bounded overshoot rather than allowing one
unbounded builtin to hide behind one bytecode instruction.

Exhaustion is a typed terminal failure. Foreground confidence builds assert
loudly; durable processes expose the stable
`process_execution_bound_exhausted` failure code and confidence builds assert
loudly there as well.

The deadline's redrive divergence is accepted as inherent to a real time bound:
a redrive may exhaust a segment tail that the original attempt completed inside
the limit. Uncommitted tail measurements are discarded on crash, so crash loops
do not accumulate time, while the instruction budget remains the deterministic
upper bound.

Adding the original instruction and deadline meters changed the continuation
layout and raised `BYTECODE_FORMAT_VERSION` from v1 to v2. Adding heap identity,
the allocation counter, live logical bytes, and size-schedule version raises it
again from v2 to v3. Deployments must drain or recreate parked Lashlang
processes before the cutover; older continuations are not migrated or decoded.

> **Historical versions.** The version numbers in this ADR record the state at ratification. The current values live in `lash::formats`; see `scripts/check_format_versions.py`.

## Consequences

- Hosts cannot accidentally confuse tool latency, instruction work, and logical
  heap growth.
- Durable segment handovers cannot reset any meter.
- Time-bound redrives are intentionally not bit-for-bit outcome deterministic;
  instruction-bound redrives remain deterministic.
- Bytecode-format rollouts are clean cutovers: drain or recreate parked
  processes rather than attempting to migrate older continuations.
