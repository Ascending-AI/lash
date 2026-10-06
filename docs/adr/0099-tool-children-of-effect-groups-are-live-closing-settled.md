# 0099: Tool calls and aggregates belong to the logical Run

## Status

Accepted. The [child and group design at the arc baseline](https://github.com/Ascending-AI/lash/blob/48f11c5fa761cabb4991e8497218bb870db83169/docs/adr/0099-tool-children-of-effect-groups-are-live-closing-settled.md)
is rationale for machinery this decision rejects, not an operating contract.

## Context

A tool call needs one owner from admission through incorporation. The owner
must survive worker and node loss, protect a committed final through its
declarations, and answer language aggregates whose losing calls keep running.
A separate execution per call, plus group services that kept their own ranks,
would duplicate the Run's ordering authority and spend several records on
every simple call. A loser that outlived its opener produced
unobserved side effects; a loser abandoned on a crash destroyed declarations
that [ADR 0042](0042-tool-attempts-are-atomic.md) protects.

## Decision

The logical Run owns every call from admission through incorporation. Ordinary
tool bodies are opaque attempts recorded as Run rows keyed by
`(owner, run, ordinal)`, each with a started row and an outcome under its
`ExecutionPolicy`
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §5 and §7).
Long work, independent lifetime, accepted intermediate state and hard isolation
use an admitted process. A physical turn, a worker or a node does not end its
logical opener.

The [tool-run contract](../architecture/tool-run-contract.md) defines K0-K10 and
maps their laws to executable owners. The following rules apply to turns,
processes and tool-bearing host operation Runs.

### 0. The lifecycle

**live.** Aggregate selection cancels nothing; a losing call keeps running,
exactly as a losing promise does in ECMA-262. Worker loss does not end an
opener: an admitted call is recovered from its recorded admission while the
opener lives. Every call's admission and outcome are rows, so no call is held
only in memory (§14).

**closing.** Entered by the logical owner's durable terminal transition (§7),
never by a worker or node dying. New admission stops, cancel-eligible
calls become cancel-decided, and every call whose final committed drains its
declarations, presentation and incorporation.

**settled.** Finalization has committed its outcome, parent end is recorded,
and retirement can follow once every dependency ends (§9).

Winner latency (when `race` resumes) and finalization latency (when the opener
reports success after closing drains its protected obligations) are separate
numbers. No successful turn latency bound exists: every protected committed
obligation finishes. **Background work that must survive `finish` is a process
the program named.**

### 1. Logical opener identity and admission (K1, L12)

**An opener is `Turn { session_id, turn_id }`, `SessionOperation { session_id,
operation_id }` or `Process { process_id }`.** `EffectOpener`
(`crates/lash-core-store/src/effect_opener.rs`) is that identity, derived once by
`EffectOpener::for_scope` from the `AdmittedScope`. It is stable across worker
attempts and owners. A process opener is the process's minted id, which is
never reused ([ADR 0107](0107-a-process-is-named-by-a-minted-id-a-start-by-its-key.md)),
so two processes are two openers. Every logical turn a shift runs is opened by
`Turn(logical run)`; a process-backed session turn runs its cells under the
process opener. A scope that is none of the three is refused with a typed
`EffectOpenerError`. Where the opener enters a key preimage,
`EffectOpener::identity_encoding` is the only encoding. A dead worker is neither
live-ended nor closed: only the durable closing fact of §7 classifies an opener.

**Whole-round admission fixes every call before execution.** It records the
owner, stable call ids, operand positions and aliases, the prepared request,
executable/preparation/presentation callback revisions, runtime retry/cancel
policy and capacity. One invalid member starts none. The author-facing
declaration is exactly `may_defer`, `intents` and `isolated`. Bindings are
recorded automatically; an unavailable admitted revision refuses with its typed
cause before a body, route or new identity is selected.

Resume loads the admitted request. It compares identity and request content
before fresh execution and never substitutes the live catalog or repeats
preparation to repair drift. Code cells rely on this per-call binding when
they resume from a VM snapshot (ADR 0132 §8).

### 2. Recorded attempts (K3)

A owns canonical prepared input; each attempt X owns its output and captures.
Coordination — retry eligibility, backoff, completion-key derivation and
deferred registration — runs in the owning actor and follows the recorded
schedule. Only the atomic attempt runs inside a body, which writes no durable
record of its own (ADR 0042). Coordination carries material references; a
proposal is not a durable acknowledgement.

A round's admission and a started row for every member commit in one
transaction before any body runs. A started `Once` member without an outcome
records `Interrupted` and never runs again; a started `Repeatable` member runs
again at the same ordinal, keeping its `ToolCallId`. A reported retry is a
record with a due time, and the next attempt takes the next ordinal. Bodies
deduplicate external side effects on `ToolCallId`. Finished members commit
their outcomes in batches, one transaction per batch.

### 3. Execution authority and accepted work

Admission retains the prepared caller environment, render record, admitted grant,
owner, lineage and cancellation authority beside the prepared request. A call
never invents an environment: resume, retry and a new owner all run from that
recorded material, not from current session policy or a fresh admission. Environment bytes stay protected under their owner through
their last retained dependency.

`RuntimeExecutionContext` is never serialized and there is no second
environment store. Semantic completion facts travel; live channels, plugin
handles and borrowed contexts do not. Claims suppress duplicate authoritative
completion; they do not make unrecorded opaque I/O exactly-once.

### 4. Commit versus cancel arbitration

**Cancellation requests are not cancellation decisions.** The final-attempt
record and the cancel disposition compete at one durable point in the Run's
rows, fenced by the owner's epoch, and D commits exactly one final-or-cancel
decision. A committed
final retains settlement ownership and is protected from later cancellation. A
cancel decision refuses any later final and any new semantic admission under
the cancelled call. Signalling the body follows the decision and cannot reverse
it. A final found after recovery is protected even if its live notification
was never published.

- **Declared starts.** The start's own admission commits under the call's
  cancel fence, so exactly one of launch admission and a cancel decision
  lands first; an admitted launch still realizes on resume
  ([ADR 0116](0116-tools-are-opaque.md) §3.2).
- **Cancel obligations.** A cancelled call whose runtime-owned source carries
  `CancelHint::CancelExternalWork` records and discharges its cancel obligation
  before it settles ([ADR 0116](0116-tools-are-opaque.md) §3.4).

### 5. Rankability and intent order (L18)

The §4 decision reserves the call's rank in the same transaction; a retried
decision allocates nothing. A call becomes rankable only after its own final
intent outcomes and presentation. The seat publishes the reserved rank, so
seats may land out of rank order, but a read is served only inside the seated
prefix: consumers observe ranks in order and never observe a call before its
declared effects happened.

A committed final is protected through its drain. Every lower committed rank
seats before a higher final issues declarations, including lower intent-free
ranks; the drain frontier is transitive across empty ranks. Declarations
finish before presentation, which precedes incorporation.

### 6. Presentation and incorporation

V owns distinct presentation bytes and incorporation. The recorded presentation
boundary folds the ordered presentation steps once; resume loads the recorded
model return and runs no completed body, hook, presenter or reducer. Bounded
attempt stream observations emit only after accepted presentation.

**Incorporation is an opener-owned, once-only mapping from each aggregate to its
incorporated rank prefix**, distinct from each aggregate's consumption cursor.
Before an externally effective continuation step, commit the chosen prefix with
that step, then apply it. Resume restores exactly that prefix and never adds
later-available settlements retroactively, which could grant process
possession earlier than the original execution did. The opener's phase
contexts share one `IncorporationLedger`; it rides each committed checkpoint
and VM snapshot, so a resumed opener never incorporates a rank twice.

Possession is the authority this protects: a started process reaches the opener
in the same realized outcome its projection is taken from. Refusing a late
completion suppresses delivery of a result, never a realized start. Losing values
stay unreturned; incorporation concerns runtime-owned facts, never a value the
program did not select.

### 7. Closing

Only the logical owner's terminal path — every final turn exit and every process
terminal, including failed and cancelled exits — records `Closing`, with its
proposed terminal disposition, before stopping admission or issuing any
cancellation. Worker and node loss do not. Recovery resumes closing whenever
that fact exists.

**Finalization is an ordered, idempotent sequence**; a crash between steps
resumes the first incomplete one:

1. settle cancellation and finish every protected obligation, presentation (§6)
   and incorporation;
2. commit the opener's outcome;
3. record parent end ([ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md));
4. release the Run's dependencies for retirement (§9).

Closing seats cancel-decided calls without joining their attempt bodies. It
waits only for committed calls that still owe declarations, presentation or
incorporation, and imposes no deadline on them. No fresh attempt runs after
close except to recover a committed obligation. Closing is not garbage
collection: it stops admission and discharges owned work, but reclaims nothing.

**Opener-close cancellation is a host lifetime contract.** Within a live opener,
letting losers run is Promise semantics. The divergence is at opener end, where
Lash fences further unprotected semantic writes; it does not claim that external
I/O already issued stops.

### 8. Rank authority and consuming-bridge resume

The Run rows in the lash store are the rank authority (ADR 0132 §5). There is
no group service, payload service or separate rank store. Admission, decisions,
ranks, protected drain, presentation and incorporation are committed facts of
the owning actor, so resume loads the same recorded prefix on whichever node
claims the actor.

An aggregate's consumer reads its recorded selection schedule. Consumption on
resume preserves the recorded prefix and never races calls again to decide an
existing rank.

### 9. Bounds and retirement (K2/K6, L09/L13/L16)

A Run never moves between executions: its rows and the VM snapshot are its
state, and a new owner continues from them. Native futures, borrowed contexts
and sockets never cross an owner change; a live inline attempt on a lost owner
follows its execution policy (§2).

**The limit and its unit (FIG-4546).** The session's `max_tool_calls` is
required host configuration with no default: recorded at creation, changed only
by the core `set_max_tool_calls` command, and read from the record by every
resume and reopen, so a changed limit never refuses accepted work. The
unit is the unique tool invocation; a timer is not counted, and operand
positions are not host work (§10 L4). A cell counts every tool call it makes; a
process counts the calls it holds at once, admitted, running or settled and
still required. Admission reserves capacity atomically and resume reuses the
reservation. A round that does not fit is refused whole, before anything is
dispatched, with `RuntimeErrorCode::MaxToolCallsExceeded` and its typed cause
`ToolCallLimitExceeded`. The refusal is the program's failure and is not
retried. Nothing is queued, paced or split. `batch` limits are
[ADR 0116](0116-tools-are-opaque.md) §2.5's.

**Retention follows dependencies.** Material is owner-qualified, role-tagged,
digest-checked and retained with the Run's rows. Acquire leases before
publishing a source or continuation reference. Settled work still required by
resume, a consumer or protected drain reserves capacity. Retire an aggregate
atomically only after every dependency ends, and keep its identity fence.
Missing, retired, corrupt, wrong-owner or unavailable-revision material gives a
typed retained-result refusal; it never starts a body.

Generic scope retirement and process consumer holds retain their own jobs.
External host tool-intent submissions retain first-outcome and owner-death
fences; they are not a second tool execution record.

### 10. Aggregate laws

**L1 — the four-way consumer mode is independent of the three-way wake policy.**
`RunAggregateWakePolicy` has exactly three variants — `First`, `FirstSuccess`, `All` —
and `all` and `allSettled` share `All`: they ask for the same thing and differ
only in how far the caller consumes. The wake policy is recorded admission
identity; the consumer mode is a caller-side loop decision and is never
recorded.

**L2 — the response algebra is total.**

- **`selected`** carries the first settlement for `race`, the first *successful*
  settlement for `any`, or the first *rejected* settlement for `all`.
- **`all-results`** carries all positions, for `allSettled` and for a successful
  `all`.
- **`exhausted-rejections`** carries `any`'s rejections in input-position order,
  including duplicate multiplicity.

**L3 — infrastructure failure and host cancellation propagate through a separate
host-control/error channel.** They never become tool rejections and never become
fabricated `allSettled` elements. A retryable infrastructure failure retains
admitted work; a terminal host failure enters opener closing.

**L4 — admission records the operand-position-to-unique-call mapping.** One call
executes and ranks once; its outcome expands to every mapped position, with
ascending input position as the alias tie-break. Unique-call exhaustion and
input-position completeness are distinct facts. Duplication is a consumer-side
mapping above unique calls, never two calls under one identity.

**L5 — already-settled operands and preparation completions form a source-ordered
immediate prefix ahead of newly admitted settlements.** All pending siblings are
still admitted before that prefix can answer (§11 clause 3). The prefix and the
mapping survive resume.

**L6 — a loser's value is never synthesized.** No `undefined`, no
`{status:"cancelled"}` smuggled into an `allSettled` array, no placeholder for a
call that did not settle.

**L7 — a Lashlang-native aggregate reports its first written rejection.** Every
Lashlang-native aggregate, the standalone list-batch included, asks for every
result (`AllSettled` at the boundary) and reports its first *written* unwrapped
rejection. Only the TypeScript `Promise.*` aggregates carry an ECMA consumer
mode. ADR 0086's comprehension rules are untouched.

`AbilityOp::ResourceOperationBatch` carries the consumer mode and answers with
`ResourceOperationBatchOutcome`'s four arms — `AllResults`, `Selected`,
`SettledValue`, `ExhaustedRejections`. Infrastructure failure and cancellation
are the ability's `Err`, which the VM raises as the uncatchable
`AggregateHostControl` terminal — no guest `catch` sees it — and a live
infrastructure error commits nothing for the cell, so the cell aborts and
recomputes from its last committed snapshot rather than committing an outcome
its aggregate never answered. The VM deduplicates a handle written twice into one
leaf and expands its outcome to every position.

### 11. Value model

**One pending-operation handle for tools and timers.** ADR 0095 made the VM's
single encoding `{__handle__: "lash", id}` with one mint/parse pair; timers use
it too.

1. **Bound arrays and duplicates.** An operand may be a literal array, an
   array-valued expression or an array held in a binding, and the same pending
   operation may appear twice. **Execution deduplicates; input positions never
   do.** Admission records the position-to-unique-call mapping (§10 L4).
2. **Operands evaluate once, in source order**, before any settlement is consumed.
   ADR 0086 fixes this for comprehensions.
3. **Every pending operation is admitted, even when a plain value decides the
   aggregate.** A `race` containing an already-resolved value still admits its
   pending siblings before the immediate prefix can answer; skipping would make
   a side effect depend on an operand's arrival order.
4. **A timer's start point is its admission, and its fulfilment value is
   `undefined`.** Admission records the timer's deadline once as a due time on a
   timer wait row (ADR 0132 §6); resume and reattachment reuse that deadline,
   and duplicate positions share the same timer. Recovery never starts a fresh duration.
5. **`Promise.race([])` never settles, faithfully.** ECMA-262 returns a
   forever-pending promise and there is no exception to catch. The dialect
   awaits aggregates in place and admits nothing for zero operands; the host
   detects an await that nothing can resolve and **fails the cell with a typed
   host-level unsettled-await error**, the analogue of Node exiting with code 13
   on an unsettled top-level await. It is not a catchable exception and not a
   registered ECMA deviation; it is a host lifetime contract, recorded in
   ADR 0062 beside opener close. The error code is
   `RuntimeErrorCode::AggregateAwaitUnsettled` (`aggregate_await_unsettled`),
   raised by the VM as the uncatchable `AggregateAwaitUnsettled` terminal.
6. **`Promise.any([])` rejects with an `AggregateError` whose `errors` is
   empty**, as ECMA-262 specifies, and admits nothing.
   `crates/lashlang/src/runtime/heap/validation.rs` refuses a non-aggregate error
   that carries `AggregateError` errors.
7. **`Promise.all([])` and `Promise.allSettled([])` return `[]`.**
8. **`AggregateError.errors` is input-ordered**, not settlement-ordered. Settlement
   order decides *which* rejection an unwrapping aggregate reports (§10 L2); it
   never reorders the collected errors.
9. **A raw process handle at an element position stays refused**, with the repair
   naming the tool: `crates/lashlang/src/runtime/vm/pending_tools.rs` carries
   `PROCESS_HANDLE_LEAF`, which tells the program to call
   `processes.await(handle)` and await that call.
10. **Async-map operands are aggregate operands.** The v1 async array driver runs
    callbacks sequentially, a registered deviation (`TS_ASYNC_MAP_SEQUENTIAL_V1`):
    result order matches Node, while callback interleaving and shared-mutation
    order can differ. Its census row indexes the deviation; it is not executable
    evidence of callback semantics.

An unawaited `sleep(ms)` mints a pending timer under the one handle encoding.
A timer carries no identity of its own, so an aggregate that holds one folds
every timer's position and duration, and the command that formed it, into the
aggregate's recorded admission: two timer aggregates at two sites are two
aggregates.

### 12. Deferred sources and process awaits (K4, L07)

**Deferred completion is one immutable source seal.** The Run arms its admitted
source, a wait row, before a body that may defer (ADR 0132 §6). A Deferred X
releases its local attempt and keeps the call pending without D or V. The
source authenticates its owner and resolver authority and seals exactly once:
`Resolved(ref)` or `Cancelled`. The row exists from minting, so a resolution
that arrives before the owner awaits finds it. No long process-attach
execution owns a wait. A descriptor never wins an aggregate and never emits
`ToolCompletion`.

A resolved seal drains protected finalization, permitted state commands and
presentation. Cancellation publishes no success commands; late completion
cannot revive cancelled work. A Deferred call's wait carries a `WaitDeadline`
recorded before the wait starts; an expired one resolves
`TimedOut { WaitDeadline }` (ADR 0132 §7). An inline attempt carries its
`ExecutionLimit`, recorded before its body starts. There is no automatic
reroute of a slow body to a process.

**`processes.await(h)` is a call on a `ProcessTerminal` source.** Selection never
cancels it: a winning timer in `race([processes.await(job), sleep(10_000)])`
leaves the losing await admitted while the opener lives, and a live opener that
crashes recovers it. Opener close releases the subscription without cancelling
`job`; the process's own lifetime is
[ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)'s.
A process terminal — success, failure or process cancellation — travels as a
payload and is converted at the await site. Cancelling the await is host
cancellation (`process_await_cancelled`) and never a fabricated process
terminal.

### 13. Usage

Cancellation can reject a call's semantic result after provider tokens were
spent. Each recorded model-call result retains the response's reported usage and
sealed attempt history (ADRs 0031 and 0032), independently of selection. A
re-sent `Repeatable` attempt is indistinguishable from a fresh one in what it
reports. Tool
settlements and incorporation carry semantic facts, with no usage ledger or
accounting delivery.

Hosts meter spend at the Provider seam (ADR 0127). They reserve before dispatch,
settle every attempt's receipt, including failed partial responses, and retain
request-id and attempt-ordinal identities for idempotent settlement. A call
whose result is committed is loaded on resume without dispatch. A crash before
the result commits can repeat external spend for a `Repeatable` call, so Lash
claims no exactly-once billing.
Unreported usage stays absent; live trace delivery remains best effort.

### 14. Hosts and deployments (K0/K8, L11/L21)

**No Run state is held only in memory.** Admission, decisions, closing and
retained material are durable rows in the lash store
([ADR 0102](0102-zero-infra-is-a-sqlite-in-memory-backend.md), ADR 0132), so
OS process death is worker or node loss and recovery reads the rows.

Hosts submit through `send()` and the engine's drive. A tool-bearing plugin task
is an operation Run with explicit completion, owned by its session actor.
Hosts obtain its Run handle and follow, cancel or read its result.
Session-lifetime work remains a process; tool-free administration retains its
command scope.

Changing kernel code never requires a drain; a node of a new build claims a
Run's actor when it reads the Run's stored formats
([ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) §1). Stored
shapes change in place during the pre-1.0 freeze. See the
[deployment guide](../operations/deploying-and-upgrading.md).

### 15. Cost evidence (L15)

Count the transactions, rows and bytes of a call through incorporation, and
serial waits and latency separately. Compare the same small Done workload at
widths 1/2/16. Group commit makes a round's finished members one transaction
per batch (ADR 0132 §5). Budgets per simple call come from the performance
comparison against the measured baseline; quiet-host release measurements
remain a separate release gate.

## Alternatives rejected

A loser that is durably host-owned and finishes after its opener ended buys
unobserved post-turn side effects: a `send_email` that loses to a timer realizes
its delivery with nobody watching.

A loser abandoned with a recorded decision on a crash past the winner destroys
recorded declarations between "final attempt recorded" and "declarations
drained", and mistakes an interrupted opener for an ended one.

An execution per call with group services keeping their own ranks duplicates
the Run's ordering authority, needs a second recovery path for every child,
and costs several records per simple call.

Rerouting a slow body to a process cannot preserve the body's execution policy
or its own transport contract; long work is an explicit process, an isolated
tool or a Pending tool, each with a bounded inline prefix (ADR 0132 §7).

## Consequences

One set of Run rows orders every tool fact an opener owns, so resume and
recovery read one record. Losing calls keep Promise semantics while the opener
lives and stop at its end. Long-lived or isolated work names a process. Cost is
measured per invocation tree rather than per service hop.

## Implementation and laws

- `crates/lash-core-store/src/tool_run/`: admission, material, Run fold, source
  seal, retention and state frontier.
- `crates/lash-core-execution/src/tool_dispatch/run_coordinator/`: attempts,
  aggregates, protected drain and Deferred completion.
- `crates/lash/src/tests/aggregate_oracle.rs`: the aggregate laws of §10 and §11
  on the product path.
- The substrate lanes persist Run rows and wait rows under ADR 0132 §5 and §6.

Storage laws use SQLite memory, SQLite file reopen and PostgreSQL. Laws run the
production runtime over a fault-injecting store with labelled commits, a
virtual clock and `SimNodes` (ADR 0132 §14). Upgrade witnesses use
synthetic-next. A receipt must report the
full test path and nonzero executed count; a listed target alone is no proof.
