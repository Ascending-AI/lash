# Bounded journals are an effect-controller obligation

## Decision

A process can execute arbitrarily many effects without requiring an unbounded single-invocation journal. `RuntimeEffectController` owns the boundary predicate and its engine-specific implementation. Restate segments a long process through bounded continuations; SQL stores hold domain state rather than effect replay. Process identity, provenance, wait identity and host observations remain independent of segment count.

### 1. The engine stays authoritative for durable execution within a segment

Restate's journal is the source of effect outcomes inside a segment. Cross-segment state is a VM continuation and the runtime ledgers needed to resume it, rather than a growing effect-result ledger. Making the engine a scheduler over a second Lash replay store is rejected because it duplicates durable execution to address an engine-specific journal limit.

### 2. The boundary trigger is step-count, decided by the controller

The run loop consults `wants_segment_boundary(progress)` at a quiescent post-effect point. Restate's construction-time `segment_effect_budget` sets the completed-effect threshold. A non-capturable point declines the budget boundary. Elapsed wait duration does not grow step count: ordinary durable waits suspend through the engine. Explicit drain handover can transfer an open signal wait through a captured continuation.

Thresholds tune liveness without changing authored results. There is no implemented wall-clock segmentation cap. Tests can force boundaries with controller construction options.

### 3. Cross-boundary state is a bounded VM continuation

The continuation retains instruction position, live slots and stacks. Its envelope retains replay ordinals, started-process ids, incorporation state, pending summaries and outstanding effect-group handles. The successor restores those fields before executing. The bound is live program data, not a universal byte limit: a program retaining an ever-growing value can grow its continuation independently of journal segmentation.

### 4. Code-version pinning and journal cost are separate concerns

Executable generations and routes determine whether a successor can resume captured work. Journal cost determines budget boundaries. Drain can request explicit handover, but code-version policy does not imply a wall-clock journal threshold. Incompatible stored generations use the engine's typed park or resume-refusal contracts under ADRs 0105, 0110 and 0115.

### 5. The handover and its requirements

Three requirements govern a boundary:

1. Committing the continuation, retiring the incarnation and scheduling its successor are one atomic unit, so nothing arrives in an unobserved gap.
2. Successor execution is idempotent under the stable process identity across segment resets.
3. No pending operation remains uncaptured at the cut. Required child identities, consumed settlement prefix and retention dependencies travel in the continuation.

The process remains non-terminal through a segment boundary. A retained handover is replay authority until the successor can resume and terminal publication permits retirement. The engine's journaled successor send makes delivery recoverable. Crash-window evidence exercises handover on the Restate server double, live Restate and the simulator; synthetic-next supplies upgrade proofs.

## Outstanding tool children at a boundary

Outstanding children do not block a capturable boundary. Otherwise repeated races against a hung child can grow one journal indefinitely. Successors reattach by retained invocation identity and continue from the captured settlement cursor under ADR 0099. Expired attachment is a typed recovery failure rather than permission to rerun a side effect.

Group admission bounds retained work per exact logical opener, including settled results still needed by replay or consumers. Reservations happen before dispatch, replay reuses them, and release requires discharging recovery and consumer dependencies. Command headroom is per executing controller/segment. Close budgets are attempt-local and do not alter committed obligations. ADR 0099 owns the full group accounting contract.

## Payload and lifecycle consequences

Segmentation bounds completed effect count, not the byte size of every result. Tool children journal their own outcomes. Intent admission bounds declarations and canonical declared-intent bytes; captured host environments travel with declarations but are outside that byte count. Tool output values have no universal core byte cap. Hosts size their engine entry limits and apply tool/provider output policy.

Restate owns invocation-journal retention. Domain-store maintenance does not manage SQL replay rows. Author-visible chaining and a generic store incarnation mechanism are rejected because they expose an engine limit to every author and host instead of keeping it with the controller.

[Boundary predicate](../../crates/lash-restate/src/controller/mod.rs), [continuation capture and restore](../../crates/lash-lashlang-runtime/src/process.rs), [workflow handover](../../crates/lash-restate/src/process/workflow.rs) and [retained handovers](../../crates/lash-sqlite-store/src/process_registry/segment_handover.rs) implement the contract.
