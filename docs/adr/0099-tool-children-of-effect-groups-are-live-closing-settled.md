# 0099: Tool calls and aggregates belong to the logical Run

## Status

Accepted. The [child and group design at the arc baseline](https://github.com/Ascending-AI/lash/blob/48f11c5fa761cabb4991e8497218bb870db83169/docs/adr/0099-tool-children-of-effect-groups-are-live-closing-settled.md)
is rationale for machinery this decision rejects, not an operating contract.

## Context

A tool call needs one owner from admission through incorporation. The owner
must survive worker loss and segment cuts, protect a committed final through
its declarations, and answer language aggregates whose losing calls keep
running. A separate invocation per call, plus group services that kept their
own ranks, duplicated the engine's ordering authority and spent several engine
records on every simple call. A loser that outlived its opener produced
unobserved side effects; a loser abandoned on a crash destroyed declarations
that [ADR 0042](0042-tool-attempts-are-atomic.md) protects.

## Decision

The logical Run owns every call from admission through incorporation. Ordinary
tool bodies are opaque, at-least-once recorded attempts in that opener's journal.
Long work, independent lifetime, accepted intermediate state and hard isolation
use an admitted process. A physical turn or process segment does not end its
logical opener.

The [tool-run contract](../architecture/tool-run-contract.md) defines K0-K10 and
maps their laws to executable owners. The following rules apply to turns,
processes and tool-bearing host operation Runs.

### 0. The lifecycle

**live.** Aggregate selection cancels nothing; a losing call keeps running,
exactly as a losing promise does in ECMA-262. Worker loss does not end an
opener: an admitted call is recovered from its recorded admission while the
opener lives. Every host journals, so no call is held only in memory (§14).

**closing.** Entered by the logical owner's durable terminal transition (§7),
never by a worker dying or a segment cut. New admission stops, cancel-eligible
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
attempts and segments. A process opener is the process's minted id, which is
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

Replay serves the admitted request. It compares identity and request content
before fresh execution and never substitutes the live catalog or repeats
preparation to repair drift. This is the per-call binding guarantee that
[ADR 0103](0103-code-cells-replay-by-re-execution-on-every-host.md) relies on.

### 2. Recorded attempts (K3)

A owns canonical prepared input; each attempt X owns its output and captures.
Coordination — retry eligibility, backoff wakes, completion-key derivation and
deferred registration — runs in the owning handler and follows the recorded
schedule, including its command prefix on replay. Only the atomic attempt runs
inside a recorded body, which emits no journal command (ADR 0042). Coordination
carries material references; a proposal is not a durable acknowledgement.

Crash redelivery keeps `ToolCallId` and attempt ordinal. Only a reported retry
advances the ordinal. Bodies deduplicate external side effects on `ToolCallId`.
A successful physical return settles every issued local X through
acknowledgement; failed-invocation recovery owns unfinished X.

### 3. Execution authority and accepted work

Admission retains the prepared caller environment, render record, admitted grant,
owner, lineage and cancellation authority beside the prepared request. A call
never invents an environment: replay, retry, a resumed segment and a successor
all run from that recorded material, not from current session policy or a
fresh admission. Environment bytes stay protected under their owner through
their last retained dependency.

`RuntimeExecutionContext` is never serialized and there is no second
environment store. Semantic completion facts travel; live channels, plugin
handles and borrowed contexts do not. Claims suppress duplicate authoritative
completion; they do not make unrecorded opaque I/O exactly-once.

### 4. Commit versus cancel arbitration

**Cancellation requests are not cancellation decisions.** The final-attempt
record and the cancel disposition compete at one durable, fenced point in the
Run's journal, and D commits exactly one final-or-cancel decision. A committed
final retains settlement ownership and is protected from later cancellation. A
cancel decision refuses any later final and any new semantic admission under
the cancelled call. Signalling the body follows the decision and cannot reverse
it. A final found after recovery is protected even if its live notification
was never published.

- **Declared starts.** The start's own journaled admission runs under the
  call's cancel fence, so exactly one of launch admission and a cancel decision
  lands first; an admitted launch still realizes on redrive
  ([ADR 0116](0116-tools-are-opaque.md) §3.2).
- **Cancel obligations.** A cancelled call whose runtime-owned source carries
  `CancelHint::CancelExternalWork` records and discharges its cancel obligation
  before it settles ([ADR 0116](0116-tools-are-opaque.md) §3.4).

### 5. Rankability and intent order (L18)

The §4 decision reserves the call's rank in the same journaled step; a retried
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
boundary folds the ordered presentation steps once; replay serves the recorded
model return and runs no completed body, hook, presenter or reducer. Bounded
attempt stream observations emit only after accepted presentation.

**Incorporation is an opener-owned, once-only mapping from each aggregate to its
incorporated rank prefix**, distinct from each aggregate's consumption cursor.
Before an externally effective continuation step, record the chosen prefix in
that step's replay history, then apply it. Replay restores exactly that prefix
and never adds later-available settlements retroactively, which could grant
process possession earlier than the original execution did. The opener's phase
contexts share one `IncorporationLedger`; it rides each journaled checkpoint and
segment handover, so a resumed opener never incorporates a rank twice.

Possession is the authority this protects: a started process reaches the opener
in the same realized outcome its projection is taken from. Refusing a late
completion suppresses delivery of a result, never a realized start. Losing values
stay unreturned; incorporation concerns runtime-owned facts, never a value the
program did not select.

### 7. Closing

Only the logical owner's terminal path — every final turn exit and every process
terminal, including failed and cancelled exits — records `Closing`, with its
proposed terminal disposition, before stopping admission or issuing any
cancellation. Worker loss and segment cuts do not. Recovery resumes closing
whenever that fact exists.

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

### 8. Rank authority and consuming-bridge replay

The Run record in the opener's journal (`crates/lash-restate/src/controller/run_record.rs`)
is the rank authority. There is no group service, payload service or separate
rank store. Admission, decisions, ranks, protected drain, presentation and
incorporation are commands of the owning invocation, so replay serves the same
recorded prefix on whichever worker retries it.

An aggregate's consumer reads its recorded selection schedule. Consumption on
replay preserves the recorded prefix and never races calls again to decide an
existing rank. A consuming bridge resumed in a successor reads the transferred
Run, not the predecessor's futures.

### 9. Segments, bounds and retirement (K2/K6, L09/L13/L16)

**A cut carries the entire logical Run.** `RequestCut` freezes admission;
`Quiescing` keeps polling issued local attempts through durable acceptance;
only then is the Run `Capturable`. A pending Deferred source can transfer while
unresolved; a live inline attempt cannot. A boundary is never declined because a
call is unsettled; declining at a non-capturable point stays correct.

`RunTransfer` carries the event and independent-attempt prefixes, earlier and
current aggregates, unconsumed and unseated finals, unranked source seals,
canonical material, state frontier, capacity, admitted environment, owed starts
and cancels, the incorporation ledger and VM continuation, for both `HandOver`
and `JournalBudget`. Successor ownership is durable before the predecessor
releases its lease, and adoption fences predecessor publication. Native futures,
borrowed contexts and sockets never transfer. Turn and process owners share the
codec while retaining their separate lifecycle transactions.

**The limit and its unit (FIG-4546).** The session's `max_tool_calls` is
required host configuration with no default: recorded at creation, changed only
by the core `set_max_tool_calls` command, and read from the record by every
replay, redrive and reopen, so a changed limit never refuses accepted work. The
unit is the unique tool invocation; a timer is not counted, and operand
positions are not host work (§10 L4). A cell counts every tool call it makes; a
process counts the calls it holds at once, admitted, running or settled and
still required. Admission reserves capacity atomically and replay reuses the
reservation. A round that does not fit is refused whole, before anything is
dispatched, with `RuntimeErrorCode::MaxToolCallsExceeded` and its typed cause
`ToolCallLimitExceeded`. The refusal is the program's failure and is not
retried. Nothing is queued, paced or split. `batch` limits are
[ADR 0116](0116-tools-are-opaque.md) §2.5's.

**Retention follows dependencies.** Material is owner-qualified, role-tagged,
digest-checked and journal-local or retained. Acquire leases before publishing a
source or continuation reference. Settled work still required by replay, a
consumer or protected drain reserves capacity. Retire an aggregate atomically
only after every dependency ends, and keep its identity fence. Missing,
retired, corrupt, wrong-owner or unavailable-revision material gives a typed
retained-result refusal; it never starts a body. A measured handover copy is
allowed and counted.

Generic scope retirement, process consumer holds and process journal pins retain
their own jobs. External host tool-intent submissions retain first-outcome and
owner-death fences; they are not a second tool execution journal.

### 10. Aggregate laws

**L1 — the four-way consumer mode is independent of the three-way wake policy.**
`RunAggregateWakePolicy` has exactly three variants — `First`, `FirstSuccess`, `All` —
and `all` and `allSettled` share `All`: they ask for the same thing and differ
only in how far the caller consumes. The wake policy is recorded admission
identity; the consumer mode is a caller-side loop decision and is never
journaled.

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
mapping survive replay.

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
controller error is also recorded as the enclosing execution's nested effect
error, so the cell aborts and is redriven rather than committing an outcome its
aggregate never answered. The VM deduplicates a handle written twice into one
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
   `undefined`.** Admission records the timer's deadline once from a journaled
   clock sample; replay and reattachment reuse that deadline, and duplicate
   positions share the same timer. Recovery never starts a fresh duration.
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
source before a body that may defer. A Deferred X releases its local attempt and
keeps the call pending without D or V. The source authenticates its owner and
resolver authority and seals exactly once: `Resolved(ref)` or `Cancelled`.
Resolve-before-subscribe reads that same seal. Short subscriptions target the
current segment; transfer rebinds them and ends the predecessor's interest. No
long process-attach invocation owns a wait. A descriptor never wins an
aggregate and never emits `ToolCompletion`.

A resolved seal drains protected finalization, permitted state commands and
presentation. Cancellation publishes no success commands; late completion
cannot revive cancelled work. Tools own transport timeouts inside their bodies.
Hosts bound Runs with cancellation and turn/no-progress limits. There is no
runtime per-call deadline, timeout terminal or automatic reroute to a process.

**`processes.await(h)` is a call on a `ProcessTerminal` source.** Selection never
cancels it: a winning timer in `race([processes.await(job), sleep(10_000)])`
leaves the losing await admitted while the opener lives, and a live opener that
crashes recovers it. Opener close releases the subscription without cancelling
`job`; the process's own lifetime is
[ADR 0094](0094-child-lifecycle-is-a-registration-fact-settled-by-scope-end.md)'s.
A process terminal — success, failure or process cancellation — travels as a
payload and is converted at the await site. Cancelling the await is host
cancellation (`process_await_cancelled` in `crates/lash-restate/src/process/mod.rs`)
and never a fabricated process terminal.

### 13. Usage

Cancellation can reject a call's semantic result after provider tokens were
spent. Each recorded model-call result retains the response's reported usage and
sealed attempt history (ADRs 0031 and 0032), independently of selection. A
replayed attempt is indistinguishable from a fresh one in what it reports. Tool
settlements and incorporation carry semantic facts, with no usage ledger or
accounting delivery.

Hosts meter spend at the Provider seam (ADR 0127). They reserve before dispatch,
settle every attempt's receipt, including failed partial responses, and retain
request-id and attempt-ordinal identities for idempotent settlement. A call
whose result is recorded is served on replay without dispatch. A crash before
recording can repeat external spend, so Lash claims no exactly-once billing.
Unreported usage stays absent; live trace delivery remains best effort.

### 14. Hosts and deployments (K0/K8, L11/L21)

**No Run state is held only in memory.** Admission, decisions, closing and
retained material are durable facts in the owning journal and its stores
([ADR 0102](0102-zero-infra-is-a-sqlite-in-memory-backend.md),
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)), so
OS process death is worker loss and recovery reads the journal.

Hosts submit through `send()` and the engine's drive. A tool-bearing plugin task
is an operation Run with explicit completion, reached through the existing
session turn service. Hosts obtain its Run handle and follow, cancel or read
its result. Session-lifetime work remains a process; tool-free administration
retains its command scope.

Hosts obtain the SDK through `lash::restate::restate_sdk` or
`lash_restate::restate_sdk`, and bind host and Lash handlers on one Endpoint.
Changed handler command order or names move `JOURNAL_LOGIC_EPOCH` and its
synthetic-next counterpart immediately. Stored shapes change in place during
the pre-1.0 freeze. The predecessor retains its generation's drain lane.

Transferred source waits must not pin an old deployment. Independently owned
old work, process pins, delivery obligations and `unfinished_invocations` still
block non-forced removal. Failure to query that evidence refuses removal; it
never proves drained. See the [deployment guide](../operations/deploying-and-upgrading.md).

### 15. Cost evidence (L15)

Count the complete invocation tree through incorporation: source records,
raw engine records, RPCs, canonical and transport bytes, application transactions,
serial waits and latency separately. Compare the same small Done workload at
widths 1/2/16. Four records for a simple singleton and `1+3N` for independent
Done calls are branch budgets, not totals for Deferred, retries, starts,
cancellation or transfer. The child-invocation route is a failing four-record
control; quiet-host release measurements remain a separate release gate.

## Alternatives rejected

A loser that is durably host-owned and finishes after its opener ended buys
unobserved post-turn side effects: a `send_email` that loses to a timer realizes
its delivery with nobody watching.

A loser abandoned with a recorded decision on a crash past the winner destroys
recorded declarations between "final attempt recorded" and "declarations
drained", and mistakes an interrupted opener for an ended one.

An invocation per call with group services keeping their own ranks duplicates
the engine's ordering authority, needs a second recovery path for every child,
and costs several engine records per simple call. A second SQL settlement
journal would make storage an effect engine.

A runtime per-call deadline that stops a slow body and reroutes it to a process
cannot preserve at-least-once body semantics or the body's own transport
contract; long work is an explicit process.

## Consequences

One journal orders every tool fact an opener owns, so replay, transfer and
recovery read one record. Losing calls keep Promise semantics while the opener
lives and stop at its end. Long-lived or isolated work names a process. Cost is
measured per invocation tree rather than per service hop.

## Implementation and laws

- `crates/lash-core-store/src/tool_run/`: admission, material, Run fold, source
  seal, transfer, retention and state frontier.
- `crates/lash-core-execution/src/tool_dispatch/run_coordinator/`: attempts,
  aggregates, protected drain, Deferred completion and capture/adoption.
- `crates/lash-restate/src/controller/run_record.rs`: owning-journal recording.
- `crates/lash-restate/src/durable_wait/source_seal.rs`: source authority and seals.
- `crates/lash-restate/src/tests/run_coordinator_on_the_double/`: aggregate,
  continuation and owner-recovery laws; `durable_wait_source_seal.rs` covers L07.
- `crates/lash/src/tests/aggregate_oracle.rs`: the aggregate laws of §10 and §11
  on the product path.

Storage laws use SQLite memory, SQLite file reopen and PostgreSQL. Execution
hosts are the in-process Restate server double, live Restate and lash-sim's
effect host. Upgrade witnesses use synthetic-next. A receipt must report the
full test path and nonzero executed count; a listed target alone is no proof.
