# Tool-run contract

This is the contract every worker of the tool execution end state (FIG-4864)
builds against. Every ordinary tool body becomes one recorded attempt in its
logical Run's opener journal. The Run owns admission, retries, the
final-or-cancel decision, protected drain, presentation, incorporation,
aggregates and surviving losers. Long, isolated or independently living work
is a process admitted before its body runs.

The seams below are pinned as types, pure transitions and codec/refusal
witnesses. They are expansion interfaces: each names the ticket that wires
it into production, and no production route reads it before that ticket
lands. The normative specification behind them is the FIG-4864 arc
specification (`tool-final-spec.md`), including the binding Q2-Q5 rulings,
the hook policy and the adopted hook-composition ruling. FIG-529's
effect-host simulator targets these seams, not `ToolChildHost`.

## Seams

| Seam | Contract | Pinned in | Witness | Implementing owner | Consumers |
| --- | --- | --- | --- | --- | --- |
| K0 | SDK `Endpoint`, contexts, serde and named `run` with a retry policy, reached only through `lash_restate::restate_sdk` | `crates/lash-restate/src/tests/tool_run_sdk_contract.rs` | compile | FIG-4870 (fork intake) | FIG-4871-4874, every host |
| K1 | Whole-round admission: owner, call ids, operand aliases, prepared request, three-capability declaration, automatic callback binding, runtime retry/cancel policy, before-check record, reserved capacity | `lash_core_store::tool_run::admission` | codec, refusal | FIG-4875 | FIG-4877, FIG-4879, FIG-4855 |
| K1/K3/K10 hooks | Tool hook phases, occurrences, verdicts, reducer and selection | `lash_core_store::tool_run::tool_hooks` | codec, reducer permutations | FIG-1399 (API cutover, ADR 0128) | FIG-4875, FIG-4877, FIG-4878 |
| K2 | Owner-qualified material references and typed retained-result refusals (Q4) | `lash_core_store::tool_run::material` | codec, refusal | FIG-4876 | FIG-4877, FIG-4883, FIG-4739 |
| K2/K6 retention | Retained bundles and their dependency leases: retain before publication, successor acquire, release, atomic retirement, holder fences | `lash_core_store::tool_run::retention`, `ToolMaterialStore` | codec, store laws (`tool_material_tests!`) | FIG-4889 | FIG-4739, FIG-4890, FIG-4883, FIG-4740 |
| K3/K9 | Run events with stable ordinals, the sole-active-segment fold, final-or-cancel once, protected drain frontier, reported-retry schedule, `Live`/`Closing`/`Settled` | `lash_core_store::tool_run::run_event` | codec, fold refusals | FIG-4877, FIG-4879, FIG-4880, FIG-4882 | FIG-4881, FIG-4892, FIG-529 |
| K4 | Immutable `Resolved(ref)`/`Cancelled` source seal, authority, short subscriptions | `lash_core_store::tool_run::source_seal`; served by `LashDurableWaitIndex` `arm_source`/`subscribe_source`/`unsubscribe_source`/`seal_source` and `LashDurableWaitWorkflow/seal_source` (`crates/lash-restate/src/durable_wait/source_seal.rs`) | codec, refusal, L07/L12 double laws (`durable_wait_source_seal`) | FIG-4883, FIG-4740, FIG-4891 | FIG-4886, FIG-4887 |
| K5 | Declared start obligation: stable `StartKey`, registration, environment, consumer hold; cancel before/after admission; drained inside a final's declarations by the Run records `StartAdmitted`/`StartLaunched`/`StartDischarged` | `crates/lash-core-execution/src/runtime/process/declared_start.rs`; `lash_core_store::tool_run::run_event` | codec, refusal, fold refusals, L08 double laws over SQLite memory and file reopen (`declared_start_run_drain_on_the_double`) | FIG-4884, FIG-4885 | FIG-4887, FIG-4888 |
| K6 | RequestCut, Quiescing, Capturable; complete Run transfer bound to its owner | `lash_core_store::tool_run::continuation` | codec, refusal, L10 protocol witness | FIG-4881, FIG-4739, FIG-4890, FIG-4889 | FIG-4891, FIG-4892 |
| K7 | One accepted and one terminal business receipt per call id; observation permits minted from records | `lash_core_store::tool_run::receipt` | transition | FIG-4830 | FIG-4848, tracing |
| K8 | Operation Run input kind over the session-operation opener (Q2) | `lash_core_store::tool_run::operation` | codec, identity | FIG-4888, FIG-4893 | host operations |
| K10 | Callback slots and state authority, command batches, resolutions, applied frontier (Q5) | `lash_core_store::tool_run::state_command` | codec, refusal, frontier | FIG-4878 (FIG-4857 for construction) | FIG-4879, FIG-4880 |

`RunCoordinator::decide_round` records one admission for the whole round,
then registers independent `RunAttemptEntry` handles in admission order.
Each X owns its canonical output. A selected Run record folds that X and
its final decision together, or records retry eligibility and backoff;
a separate recorded timer wake registers the next ordinal. Served replay
reconstructs this command prefix before awaiting an older unfinished X.
The independent Done path costs one A and one X/D/V per member.

Admission pins a material reference to the owning plugin namespace at its
recorded generation. The Run retains one canonical image per namespace
frontier; body requests share a read-only view of that image. Crash replay
and reported retries resolve the recorded reference and keep that snapshot.
Successful X captures hold declared state commands as data. Only the
selected successful final reduces them, records the P58 resolutions with
D, and publishes after D is durable. Failed or cancelled candidates
publish no success commands; replay installs the recorded resolutions
without executing a body, check or reducer.

`RunCoordinator::start_round` registers issued attempts with the coordinator;
`progress` accepts one recorded selection at a time. A program effect can
therefore run after a winner while another body remains unfinished.
`request_cut` freezes further admission, and `quiesce` continues the same
recorded schedule through durable acceptance of all issued local work.
`capture_cut` refuses a pending handle, a failed invocation or a dropped
progress frame. A body proposal is never an acknowledgement. Registered
retry work retains its existing schedule and a pending Deferred source
needs no local waiter.

`RunTransfer` is the shared capture and adoption codec. It carries the
acknowledged journal and independent attempt receipts, resolved state and
namespace frontiers, unconsumed decisions, source authority, admitted
environment, held capacity and declared-start obligations. `retain_cut`
leases canonical material before publication and removes payload bytes from
transferred receipts. `RunCoordinator::adopt` acquires the successor lease,
rebuilds the recorded fold and obligations, and fences predecessor append
before the successor writes its first record. It carries no native handle and
performs no Closing, cancellation, reroute or successor publication. Turn
and process continuation integration belongs to FIG-4739 and FIG-4890;
source transfer belongs to FIG-4891. Production aggregate callers activate
the coordinator in FIG-4894 and FIG-1863. Stored format versions remain frozen.

Process admission binds a `SegmentOrdinal` onto execution authority without
changing its process-attempt fence. The native segment envelope carries the
same `RunTransfer` beside the opaque VM continuation. Capture checks local ACK,
retained material, event and capacity frontiers. Restore checks the process,
admitted successor and inherited environment before loading definitions.
Publication stores the original predecessor-stamped transfer unchanged.
Coordinator adoption rebinds subscriptions and reads material only through
successor-held references, including after the predecessor lease is fenced.
The process registry still owns lifecycle transactions and child holds.

The state frontier carries each acknowledged publication's receipt by ordinal.
After adoption, an identical historical receipt remains `AlreadyApplied`.
A changed receipt at that ordinal yields `FrontierRefusal::ReceiptMismatch`,
including when its original publisher has already been fenced.

Each physical process invocation keeps its fixed journal pin. A terminal
publishes its lifecycle outcome before releasing that pin. A handover persists
the successor state and accepts its send, registers the successor invocation's
fixed pin, then releases the predecessor pin. Pending sources add no per-call
pin or deadline. The process journal logic epoch changes with this command
prefix; a predecessor journal retains its original generation's drain lane.

`RunCoordinator::start_aggregate` records unique leaves, source positions,
aliases and timer admission time with the round's A. Pending siblings are
registered before an immediate prefix can answer. Consumer modes remain
caller policy: `race`, `any`, `all`, `allSettled` and the list batch observe
the same recorded decisions and timer wakes. `all` reports the first terminal
rejection; the list batch waits for every leaf and reports the first rejection
in written order. Duplicate operands execute and consume one unique call.
An empty race and a pending Deferred source carry no fabricated result.

`consume_aggregate` exposes only the values selected by that consumer. Losing
calls remain Live under the Run. `beside` polls issued X handles alongside a
program effect, and protected presentation polls those handles while it drains.
`drain_protected` records presentation and incorporation without consumption;
a later consumer records its own `Consumed` fact. Coordination retains material
references, rather than storing another copy of the loser's output.

The aggregate owner supplies its clock. Timer admission records the original
instant; recovery registers the remaining wait through the existing Restate
timer facility.

Only `close` ends the logical Run: it records Closing, freezes admission,
discharges admitted eligible cancellation, accepts every issued X through its
durable ACK, drains accepted finals and records Settled. Ignore-policy work
receives no external cancel. Worker loss leaves recovery to the original engine
journal. A physical cut retains Live and transfers aggregate plans, deadlines,
unconsumed material and pending source descriptors through its existing snapshot.
Production aggregate callers remain assigned to FIG-4894, FIG-1863 and FIG-4895.

The plugin registrar mints every callback key from `CallbackSlot`, so a
callback slot cannot exist without its key prefix and its state authority.

## Binding rulings the seams encode

**Declaration (Q3).** The author declares exactly `may_defer`, `intents` and
`isolated`, as the `ToolDeclaration` on the tool's `ToolManifest`
(`lash_sansio`, re-exported by `tool_run::admission`). Admission records it
with the manifest the call is admitted under — the catalog's, the grant's, a
replayed cell's recorded binding or a group child's retained admission — and
dispatch reads only that record; no provider hook is consulted after
admission (FIG-4875). A round's calls are admitted together before any
prepares: an invalid declaration or an isolated declaration with no bound
process implementation refuses every member with a typed
`ToolAdmissionRefusal`. An outcome the record does not admit — Deferred
without `may_defer`, an undeclared intent kind — fails the call with
`ToolFailureCause::Declaration` before anything it declared is realized. An isolated call is a process from its start with no inline body,
so it declares neither `may_defer` nor intents. There is no per-call
timeout, duration, budget or idempotent capability, and the declaration
refuses those fields when decoding. Retry and cancel policy are recorded
runtime policy. Crash recovery is at-least-once under the stable
`ToolCallId` and attempt ordinal; only a reported retry advances the ordinal.
`ToolCallId` is the external idempotency key.

**Binding (FIG-4854).** Admission binds the executable, preparation and
presentation callbacks as `PluginCallbackIdentity { owner: PluginRevision,
key }`. A resumed call whose bound plugin revision is unavailable refuses
with `PluginExecutionRefusal` (`plugin_revision_unavailable`) before any
body, route or identity is chosen.

Presentation binds an optional singleton presenter followed by ordered steps.
Every entry includes the exact callback key and owning revision. An empty
plan is explicit and does not adopt callbacks installed later. An owed
presentation resolves the entire plan before invoking any callback; missing
keys and changed revisions retain the typed callback refusal. Completed
presentation replay serves its recorded return without resolving callbacks.
The current scalar and child completion callers record selection through
`LanguageRuntimeValue` before `PresentToolResult`; the Run admission owner
consumes the same K1 binding when it replaces those callers.

**Material (Q4).** A reference carries owner (Run, process or source), role
(prepared request, attempt output, presentation), location (journal-local or
retained artifact) and a digest. Retention moves bytes, never identity. A
failed read is a typed `MaterialRefusal` and never re-executes a body.

FIG-4876 extends the existing opener dictionary with `MaterialEntry` and
`MaterialPayload`. A canonical entry owns its text, format and optional
plugin codec revision. Its BLAKE3 digest under `lash-tool-material/v1`
covers all of those fields, including owner and role. Coordination stores
the reference and its position in the original envelope. Cold replay restores
the original JSON token order and checks owner, integrity, format and codec
revision before exposing a recorded result. Native UTF-8 needs no plugin
decoder. A distinct presentation creates material only when its bytes differ
from bytes already recorded by A or X.

The retained artifact uses the same `MaterialEntry` codec. Moving a reference
to an artifact changes location alone and preserves its digest. A retired
entry carries its reference without text. Missing, retired, corrupt,
wrong-owner, wrong-role, unsupported-format and unavailable-revision reads
carry `RuntimeErrorCause::MaterialRefused` with the original `MaterialRefusal`
under terminal `retained_result_refused`; no refusal grants execution authority.
Resolution stays inside the controller. The status-only drive reply remains
unchanged.

**Retention (FIG-4889).** Same-segment material resolves from the opener
journal and costs no artifact transaction. Material another segment or a
Deferred source seal names is retained first: `MaterialBundle` packs the
payloads into one immutable bundle in the `ToolMaterial` artifact store,
named by its bytes under `lash-tool-material-bundle/v1`, and
`ToolMaterialStore::retain_material` writes it together with the holder's
lease, a `run_segment` or `source` referrer edge, in one transaction. Only the
`RetainedBundle` that returns may be published: `RunTransfer::check_capture`
refuses material outside a tool-material bundle (`UnretainedMaterial`) or
held by another lease than the transferring segment's (`UnleasedMaterial`),
and a seal refuses an unretained result (`UnretainedResult`). The successor
acquires its own lease before it reads and before the predecessor releases,
so the predecessor's lease lasts until successor ownership is durable. A
release fences its holder, severs its leases and retires every bundle with
no lease left, all payloads at once. The holder fence is the identity fence:
an ended holder cannot republish, reacquire or read, its references refuse
`Retired`, and a retired bundle refuses `Missing` to every later holder.
Closing a Run is not garbage collection; only a release ends a lease.
`RetainedBundle::copy_bytes` reports the measured handover copy.

**Operation (Q2).** A tool-bearing host operation is a Run with its own input
kind, driven by the session's keyed turn service, over the existing
session-operation opener. Its call ids and start keys keep their bytes.
FIG-4888 admits a host's plugin task at the head of the command lane as
`AdmittedWork::Operation`, under the run `OperationRun::run_id` names
(`shift-operation:<batch>`), so every admission of the operation, a redrive
after a crash included, names the same run and the same `LashTurn` key. The
host command returns once the command row and its ingress obligation are
durable; the command run stops at a task. The operation run's invocation is
the journal owner: `own_effect_controller_task` hands the task a proxy of
that invocation's controller, rescoped to the operation's session-operation
scope, and serves it `Live`, then `Closing` once the task returned (nothing
new admitted) until every issued effect settled, then `Settled`. The pre-run
cancel peek is recorded there, so a replay takes the same branch. FIG-4893
moves host result, follow and cancel onto the operation run.

**State (Q5) and hook policy.** Only before-turn, after-turn, checkpoint and
after-tool (result check) callbacks on the Run's sequential path may return state commands; every other
callback is decision-only. Commands are reduced privately, recorded with their
predecessor, published after durable acceptance, and replayed without running
a body, hook, reducer or converter. One refusal publishes nothing.

**Hook composition.** For one admitted call: argument transforms, provider
preparation, then every before-check on one immutable prepared call. Checks
reduce by AbortRun > Deny/Cancel > CachedSuccess > Allow, ties broken by
ascending UTF-8 plugin id and then callback key. A cached success is data
only and still passes the result transforms and after-checks. After-checks
return only Allow, Deny, Cancel or AbortRun and never replace a result.
AbortRun fails the call and stops the owning logical Run: the fold refuses
any later admission and retry. Every reply is recorded with its callback, in
reduction order; a recorded record is served, never re-reduced. Each reply
is keyed by its occurrence: admission, attempt ordinal, Deferred completion
of an attempt, or cached.

**Run records (K3, FIG-4877).** A Run record is one journaled step of the
owning handler, `lash:run:{call}:{step}`, holding a `RunJournalEntry`: the
record and the canonical material it owns, stamped with the effect-journal
generation. Effect controllers journal it through
`RuntimeEffectController::record_run_record`; a replay serves it without
running its step, and every served record passes the `RunLedger` fold and the
material check before anything acts on it. The singleton route
(`lash_core::tool_dispatch::run_singleton_tool`) records a simple Done call as
four records: `admit` (A: the prepared request and every before-check),
`attempt:1` (X: the body's capture, checked against the recorded declaration),
`decide` (D: after-checks, or the Run's cancellation read once) and `present`
(V: presentation bytes distinct from the output, consumed and incorporated in
the same record). A final that declares adds `declare` between D and V, and V
then also settles the declarations. A replay whose call drifts from its
recorded admission (tool name, arguments, owner, or a recorded plugin revision
this build no longer executes) refuses typed before any body; the recorded
declaration governs, never the live catalog. The route is an expansion
interface: production rounds keep their route until FIG-4894, FIG-1863 and
FIG-4895 move them. Reported retries are FIG-4879's schedule.

**Deferred completion (K4, FIG-4740).** The Run arms a source under the
logical opener before starting a body admitted to defer. Its Deferred X
retains the matching source key and leaves the call open without D, rank or
V. `await_deferred` subscribes at the Run, reads a Resolved seal's canonical
retained result under the source lease, and accepts its decision before
`drain` presents it. Cancellation seals the source first and accepts the
actual winner; a Resolved winner remains protected through after-checks and
presentation. A handover preserves the open source. Runtime per-call
deadlines, timeout results and timer races are absent; body-owned transport
failures, Run cancellation and Run limits retain their own semantics.
These calls use the new journal logic generation; production round
activation remains with the integration tickets named above.

**Protected drain (K3, FIG-4880).** `lash_core::tool_dispatch::RunCoordinator`
runs several calls in one logical Run, each admitted as a singleton round.
`decide` records a call's A, X and D; the decision takes the Run's next rank
(`RunLedger::next_rank`, from 1), so ranks follow the order decisions became
durable. `drain` then works through every decided call in rank order. A final
whose result declares intents issues them (`declare`) only once every
committed final ranked below it is seated (`RunLedger::drain_frontier_open`),
realizes them behind their exactly-once fences inside its `present` step,
and settles them in the same record as its presentation and incorporation.
An intent-free final seats at its decision without waiting, so its seat
certifies nothing about lower ranks: the frontier is every lower rank, never
only the one just below (L18), and the fold refuses a declaration issued
early with `DrainFrontier`. Presentation and incorporation follow rank order,
so the incorporated calls are a rank prefix. The Run's cancellation is read
only inside a decision's step: a final decided before it still drains, and a
call decided after it is cancelled and declares nothing. A Deferred attempt
takes no rank and no presentation; its descriptor grants neither value nor
place in the drain. Records are appended one at a time in program order;
an effect the caller issued before the drain keeps progressing while a
final's declarations are held. Concurrent attempts and their recorded
schedule are FIG-4879's. The laws are
`crates/lash-restate/src/tests/run_coordinator_on_the_double.rs`, including
the drain-transitivity oracle ported from `effect_group_drain_transitivity`.

**Attempt stream (FIG-4880).** The bounded stream a body emits belongs to its
attempt's capture (X): `SingletonAttempt::stream` is an
`AttemptStreamRecorder` observation sink, and the capture carries the
`AttemptStream` it records, with deltas of a block coalesced, shared call
fields stored once and the bytes capped by `ATTEMPT_STREAM_BYTE_BUDGET` under
a typed `AttemptStreamTruncation`. The Run emits it after the presentation record is durably accepted
(`SingletonToolHandlers::emit_stream`); an unacknowledged proposal emits
nothing, and a replay that serves the presentation emits nothing again.
A declared presentation refusal records the original result as fallback
alongside its typed `HookCause`; an invocation fault leaves V uncommitted
for engine recovery. A lost V acknowledgement may omit the observation. The tool-child settlement still carries the same
representation until FIG-4899 removes that transport.

**Declared starts (K5, FIG-4884).** A final may declare one process start.
Its attempt record owns the start's obligation as material: the body's
registration under its stable start key, bound by the Run to the Run's
environment when lash executes the process (an externally owned one runs
under none) and to a consumer hold, owned by the Run's opener, that carries
the call's recorded cancel policy. A keyless start, or a lash-executed one in
a Run that owns no environment, is the attempt's typed `StartRefused`; a start the admitted
declaration does not name is its `UndeclaredIntent` refusal. The start
drains inside the final's declarations: `declare` admits it with them
(`StartAdmitted`), `start:launch` registers it under its key
(`StartLaunched`), and `start:discharge` reads the Run's cancellation once,
cancels the process when the recorded policy says so, and releases the hold
(`StartDischarged`), before `present` settles the declarations. A
cancellation before the decision is durable withholds the final, so its
start is never admitted. One after it cannot forbid the start: a lost launch
registers again under the same key and recovers the same process. The fold
refuses a start outside its issued declarations, a second launch or
discharge, a key another start of the Run holds, and settled declarations
while a start is owed; `RunLedger::owed_starts` names what a successor
segment owes. Registration arms the process's start obligation, which the
process outbox delivers; production park sites keep their launch until
FIG-4740 turns them into Deferred attempts.

**Process-backed Deferred starts (D06, FIG-4887).**
`SingletonBodyOutcome::DeferredStart` records one K5 obligation in X, with a
reserved process-terminal source distinct from the external completion key.
The protected start drain decides cancellation before admission, then records
`StartAdmitted` and `StartLaunched` under the stable `StartKey`. It arms a K4
`ProcessTerminal` descriptor using the minted `ProcessId`, and the call stays
open without a rank. Short terminal registrations retain records at the process
and receiver indexes; no `ProcessAttach` or terminal-wait invocation runs.
Delivery acquires the receiver's attachment ownership, retains the canonical
capture and seals the source through the existing K4 writer. The Run accepts
that immutable winner before discharging the start's policy and consumer hold.
Session-owned subagents record `Ignore` for cancellation of their observing turn;
starter-owned work records `CancelExternalWork`. `processes.await` observes work
with `Ignore`. Production round activation remains F01's responsibility.

## Field ownership

| Record | Owns | Refers to |
| --- | --- | --- |
| A, admission | prepared request material, declaration, binding, policy, before-check record, operand slots, capacity | owner opener |
| X, attempt | attempt output and captures, or the Deferred source key | attempt ordinal |
| D, decision | final-or-cancel, rank, after-check record, declarations flag, resolved state batch | X or the cached result |
| V, presentation | presentation bytes distinct from the output, incorporation | D |
| Source seal | the resolved result, owned by the source | source key |
| Run transfer | event prefix, retained material, subscriptions, owed starts and cancels, state frontier, capacity, VM continuation flag | owner opener |

Coordination records hold references, never payload copies. A handover may
copy material into a retained artifact; that copy is counted, and it is not
a second canonical owner.

## Identity preimages

Stored shapes change in place before the 1.0 cut; identities do not move
with them. The goldens in
`crates/lash-core-store/src/tool_run/identity_tests.rs` pin these preimages
byte for byte (removal row M0201):

- `ToolCallId`: `tc_` plus BLAKE3 under `lash-tool-call-id/v1`, rooted in the
  opener's admission (ADR 0117 §2).
- Opener encodings: `turn:`, `drain:` (session operation) and `process:`,
  each component length-prefixed.
- `StartKey`: `process-start-key:v1:<namespace>:blake3:<hex>` for the
  intent, trigger, host and keyless families; keyless keys take scope tags
  1 turn, 2 process, 3 session operation, 4 session delete and
  5 runtime operation.

A change that moves one of these is an identity change, never a shape change.

## Journal generation lanes

A build's drain generation hashes every drain-surface format version,
`JOURNAL_LOGIC_EPOCH`, the session admission window and the ordered plugin
composition (ADR 0106 §1). Every landing that changes a handler's journaled
command structure — what it records, the order of its records, or a step's
name — moves that handler's lane in the same commit:

| Handler | Lane | Moves with |
| --- | --- | --- |
| `LashTurn` `run`/`close` and the Run's A/X/D/V records | pinned | `EFFECT_JOURNAL_VERSION` for recorded effect bytes or positions; `LASH_SESSION_SHIFT_VERSION` for run requests and replies |
| `LashSession` `shift`, including the operation input kind | pinned | `LASH_SESSION_SHIFT_VERSION` |
| `LashProcessWorkflow` segments and declared starts | pinned | `RESTATE_PROCESS_JOURNAL_VERSION`, `PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION`, and `EFFECT_JOURNAL_VERSION` for the effects it records |
| `EffectGroupDispatch` | generation only | `EFFECT_GROUP_DISPATCH_JOURNAL_VERSION`; no new structure, removed by FIG-4900 |
| `LashDurableWaitWorkflow` source seals | shared | `DURABLE_WAIT_REGISTRY_FORMAT_VERSION` covers indexed source state; key-only wait requests carry no deadline version |
| Step order or names with unchanged bytes | all pinned | `JOURNAL_LOGIC_EPOCH` |

Shared object state keeps its stamped coexistence rules and has no lane.
The release replay corpus (`crates/lash-restate/src/tests/replay_corpus.rs`)
refuses an added or reordered step under an unchanged generation. Moving a
lane is the one version change the pre-1.0 freeze still requires: stored
shapes change in place, but a journal never replays under another command
structure. No revision replays old and new structure together. FIG-4862's
`unfinished_invocations` drain hold keeps the old deployment registered until
its invocations finish.

The landings that change command structure are FIG-4876, FIG-4877, FIG-4879,
FIG-4880, FIG-4882, FIG-4883, FIG-4740, FIG-4884 through FIG-4888, FIG-4893,
FIG-4894, FIG-1863, FIG-4895, FIG-4855 and FIG-4878. Each owns its lane move.

## Laws

| Law | Subject | Owners | Tiers |
| --- | --- | --- | --- |
| L01 | Independent parallel receipts | FIG-4871, FIG-4879 | Double first; live SDK |
| L02 | Partial-result and proposal replay | FIG-4871, FIG-4872, FIG-4877, FIG-4879 | Double and live SDK; stores when retained |
| L03 | Final and cancel choose once | FIG-4871, FIG-4877, FIG-4879, FIG-4880 | Double; live cancellation |
| L04 | Protected final precedes effects and observations | FIG-4880, FIG-4830 | Double; store tiers for intents |
| L05 | Aggregate semantics remain distinct | FIG-4879, FIG-4882, FIG-4894, FIG-1863, FIG-4895 | Double; protocol tests |
| L06 | Losers and program effects keep progressing | FIG-4882, FIG-4881, FIG-4739 | Double; handover stores |
| L07 | Deferred terminals are immutable | FIG-4883, FIG-4740, FIG-4891 | Double, SQLite memory/file-reopen, PostgreSQL; live transfer |
| L08 | Declared starts have one recoverable identity | FIG-4884, FIG-4885, FIG-4887, FIG-4888 | Double and all store tiers |
| L09 | Continuation carries the entire logical Run | FIG-4739, FIG-4890, FIG-4889 | SQLite memory/file-reopen, PostgreSQL, double and live |
| L10 | Cancelled continuation cannot infect a fresh Run | FIG-4739, FIG-4893; FIG-4867 protocol witness | All store tiers; protocol witness |
| L11 | Old deployments can drain while transferred sources stay pending | FIG-4891 | Double plus live deployment gate |
| L12 | Admission, binding and material cannot drift | FIG-4875, FIG-4876, FIG-4855, FIG-4857, FIG-4878, FIG-4889; FIG-4867 codec witnesses | Codec witnesses, double, relevant store tiers |
| L13 | Retention is bounded without resurrection | FIG-4889, FIG-1509, FIG-4900 | SQLite memory/file-reopen and PostgreSQL |
| L14 | Receipts and usage describe logical facts | FIG-4830, FIG-4852 | Double and tracing fixtures |
| L15 | The full cost is counted | FIG-4868, FIG-4876, FIG-4878, FIG-4905 | Controlled double/live measurement |
| L16 | A physical cut waits for local durability | FIG-4881, FIG-4739, FIG-4890 | Double plus SQLite memory/file-reopen, PostgreSQL and live handover |
| L17 | Reported retries replay their dynamic schedule | FIG-4879 | Double first; live SDK |
| L18 | Protected drain is transitive across empty ranks | FIG-4880 | Double; intent store tiers |
| L19 | Overlapping plugin state never depends on unrecorded work | FIG-4878, FIG-4857 | Double and SQLite memory/file-reopen/PostgreSQL checkpoint tiers; live ACK witness |
| L20 | Park recovery needs no group catalog | FIG-4892, FIG-4898 | All three store tiers plus double |
| L21 | Every intermediate command and surface change has a compiling generation closure | FIG-4867, FIG-4870, FIG-4873, FIG-4894, FIG-1863, FIG-4895, FIG-4897 through FIG-4903 | Owning kiln check/test, schema/facade gates; live old-route drain |
| L22 | Runtime never invents a per-call timeout | FIG-4875, FIG-4740, FIG-4886 | Double, schema/facade witnesses and continuation store tiers |

## File inventory

`tool-run-inventory.tsv` beside this document lists all 742 files the end
state touches: 55 delete, 608 edit and 79 survive with a recorded job. Each
row names its owning ticket, the earlier tickets whose closures stage edits
in the same file, the matched families and the replacement or surviving job.
Line anchors are intake anchors at 48f11c5fa7; owners re-run the census at
intake, classify new hits and inspect every match in their files. The
hook-composition removal rows HC01-HC17 belong to FIG-1399, FIG-4855,
FIG-4856 and FIG-4878.

Completion means no unresolved row. A survivor keeps an explicit job and is
disconnected from removed transport. A contraction removes a definition with
its imports, re-exports, constructors, trait methods, exhaustive matches,
format tables, guards, codecs, tests, corpus loaders, generated schemas and
target membership in one compiling landing.

An isolated call binds `IsolatedToolStart` at admission, including the registered
process implementation's callback revision, execution boundary and stable start.
Admission owns its canonical obligation material. The attempt references it and
runs no ordinary body. The existing protected start drain recovers one process
under that key and returns `IsolatedProcessDescriptor`. A missing implementation,
an unavailable revision or an unsupported physical boundary refuses before a
body or new process identity.

`ProcessInput::Engine` alone promises independent invocation lifetime. A hard
isolation claim requires the registered engine's `PhysicalProcessWorker` contract.
On cancellation that implementation terminates and reaps its worker, or recovers
its retained `WorkerTerminationReceipt`. The discharge journals that receipt
before the descriptor is presented, and releases the consumer hold only after
termination. Replay cannot replace the recorded implementation with a newly bound
engine. Ordinary tool bodies keep cooperative duration semantics and their own
transport timeouts; neither slow execution nor a reported timeout reroutes one
as a process.
