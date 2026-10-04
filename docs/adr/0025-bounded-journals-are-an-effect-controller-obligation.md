# Bounded journals are an effect-controller obligation

## Decision

A process can execute arbitrarily many effects without requiring an unbounded single-invocation journal. `RuntimeEffectController` owns the boundary predicate and its engine-specific implementation. Restate segments a long process through bounded continuations; SQL stores hold domain state rather than effect replay. Process identity, provenance, wait identity and host observations remain independent of segment count.

### 1. The engine stays authoritative for durable execution within a segment

Restate's journal is the source of effect outcomes inside a segment. Cross-segment state is a VM continuation and the Run event and state frontiers needed to resume it, rather than a growing effect-result ledger. Making the engine a scheduler over a second Lash replay store is rejected because it duplicates durable execution to address an engine-specific journal limit.

### 2. The boundary trigger is step-count, decided by the controller

The run loop consults `wants_segment_boundary(progress)` at a quiescent post-effect point. Restate's construction-time `segment_effect_budget` sets the completed-effect threshold. A non-capturable point declines the budget boundary. Elapsed wait duration does not grow step count: ordinary durable waits suspend through the engine. Explicit drain handover can transfer an open signal wait through a captured continuation.

Thresholds tune liveness without changing authored results. There is no implemented wall-clock segmentation cap. Tests can force boundaries with controller construction options.

### 3. Cross-boundary state is a bounded VM continuation

The continuation retains instruction position, live slots and stacks. Its envelope retains replay ordinals, started-process ids, incorporation state, pending summaries and the complete `RunTransfer`. That
transfer retains earlier/current aggregates, unconsumed and unseated finals,
unranked source seals, retained material leases, state frontier, capacity and
owed process starts and cancels. The successor restores those fields before executing. The bound is live program data, not a universal byte limit: a program retaining an ever-growing value can grow its continuation independently of journal segmentation.

### 4. Code-version pinning and journal cost are separate concerns

Executable generations and routes determine whether a successor can resume captured work. Journal cost determines budget boundaries. Drain can request explicit handover, but code-version policy does not imply a wall-clock journal threshold. Incompatible stored generations use the engine's typed park or resume-refusal contracts under ADRs 0105, 0110 and 0115.

### 5. The handover and its requirements

Three requirements govern a boundary:

1. Handover consists of ordered durable steps in one workflow handler. The handler records the successor reference and continuation, journals the successor send, forwards cancellation and retires the preceding continuation. Each store write is idempotent and the send belongs to the engine journal. After a crash, replay completes the sequence and reaches exactly one logical successor scheduling. Recovery produces the complete handover without an observable half-handover or a lost continuation. These steps span storage transactions.
2. Successor execution is idempotent under the stable process identity across segment resets.
3. No issued local attempt remains unacknowledged at the cut. Stop new admission,
   quiesce through durable ACK, then transfer pending sources, consumed prefixes,
   protected drain and retention dependencies with the continuation. A physical
   exit never records logical Closing (L09/L16).

The process remains non-terminal through a segment boundary. A retained handover is replay authority until its resume step journals the continuation. After successor scheduling and cancellation forwarding, the predecessor retires the continuation it resumed from. Its replay uses the journaled resume value even after that retirement. The successor's continuation remains retained through terminal publication until pruning permits deletion.

The [build-roll handoff laws](../../crates/lash-restate/src/tests/segment_generation_handoff.rs) exercise crashes after the continuation write, before successor send, during cancellation forwarding after send and after retirement. The [handover crash-cut laws](../../crates/lash-restate/src/tests/segment_generation_handoff/crash_cuts.rs) run the continuation-write, retirement, before-send and immediately-after-send cuts with forced replay over SQLite memory, SQLite file and PostgreSQL. They assert one successor invocation, exact continuation restoration on every replay and one retained terminal outcome. The [segment redrive law](../../crates/lash-conformance/src/conformance/segment_redrive.rs) checks recorded effects within a segment; the [simulator process crash cases](../../crates/lash-sim/src/crash_matrix/cases/process.rs) check process start and terminal recovery.

## Outstanding Run work at a boundary

An inline attempt blocks a successful cut until its result is durably accepted.
A Deferred X releases the local attempt and transfers its pending source without
waiting for resolution. `request_cut`, `quiesce` and `capture_cut` distinguish
those cases; a worker loss leaves recovery on the predecessor journal rather
than exporting a native future. The successor adopts the same logical owner.

Run admission bounds retained work, including settled results still needed by
replay or consumers. Reserve before issuing attempts, reuse the reservation on
replay, and release only after recovery, consumer and material dependencies end.
Command headroom belongs to the executing segment. Closing does not reclaim
material. Typed retained-result refusal never authorizes a fresh body. See
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md).

## Payload and lifecycle consequences

Segmentation bounds completed effect count, not the byte size of every result.
The owning Run journals each independent attempt outcome. Intent admission allows at most 32
declarations, 16 of one kind, and 64 KiB of canonical intent JSON per completed
attempt. The byte bound measures the complete declaration, including its
captured environment's digest. The capture lives once in the content-addressed
process environment store and durable referrers keep it available for replay
and realization (ADR 0113). Tool output values have no universal core byte cap.
Hosts size their engine entry limits and apply tool/provider output policy.

Restate owns invocation-journal retention. Domain-store maintenance does not manage SQL replay rows. The process event log grows with the process rather than with a segment; hosts release its payloads through the ADR 0023 release lever while the process runs. Author-visible chaining and a generic store incarnation mechanism are rejected because they expose an engine limit to every author and host instead of keeping it with the controller.

[Boundary predicate](../../crates/lash-restate/src/controller/mod.rs), [continuation capture and restore](../../crates/lash-lashlang-runtime/src/process.rs), [workflow handover](../../crates/lash-restate/src/process/workflow.rs) and [retained handovers](../../crates/lash-sqlite-store/src/process_registry/segment_handover.rs) implement the contract.
