# 0101: One session ingress carries every admitted item

## Status

Accepted 2026-09-23 (FIG-3540) as the design freeze for the one-ingress
cutover. **Not yet implemented**: the FIG-3540 cutover PR series builds it, after
FIG-3532, FIG-3513 and FIG-3531 land. The pending follow-on (§3) is implemented
(FIG-3542), ahead of the table cutover; its note says what differs until then.
Nothing else below describes current behaviour unless it says so. The FIG-3540 arc note is the frozen design this ADR records.

Supersedes [ADR 0010](0010-pending-turn-input-is-admission-evidence.md).
Strengthens [ADR 0069](0069-durable-acceptance-is-the-sole-turn-ingress.md).
Amends [ADR 0029](0029-claims-are-generation-fenced-under-the-session-lease.md),
[ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md),
[ADR 0046](0046-process-transitions-are-events-record-is-a-fold.md),
[ADR 0067](0067-durable-rows-name-one-owner-and-one-reclaim-trigger.md),
[ADR 0077](0077-session-state-migrates-totally-at-admission.md), both ADR 0097s
([commit
identity](0097-commit-identity-families-mint-frozen-unframed-preimages.md),
[durable session](0097-durable-session-and-live-session-are-two-authorities.md)),
[ADR 0098](0098-one-owner-per-sql-table-across-both-stores.md) and the
`CONTEXT.md` glossary; each carries a short note pointing here. ADR 0016,
ADR 0023, ADR 0081 and ADR 0099 are unchanged.

The owner rulings are recorded on the FIG-3540 arc (E1, E2, E4, D2, D14, F;
Sam, 2026-09-23). A later ruling the same day replaces G: session commands
become a class-level lane applied at turn boundaries (§4). The design changes
D1–D17 come from the ingress prospect round
(`/workspace/notes/lash/prospect-ingress-2026-09-23.md`), benchmarked against
Temporal, Pekko, Restate, DBOS and LangGraph. Where a review or the prospect
report differs from an owner ruling, the ruling is what this ADR records.

Amended 2026-09-24 (FIG-3600; Sam's rulings on that ticket). The *Amendment
(FIG-3600)* section below makes the ingress the only way a turn starts and the
backend's work driver the only thing that runs one: the caller submits with
`session.send(..)` and observes through a handle. It also makes the session
model durable session config changed by a session command. **Not yet
implemented**: FIG-3600 builds it inside the FIG-3540 cutover series, after
FIG-3585. Where the amendment and an earlier section disagree, the amendment
wins; the passages it overrides carry a short note.

Amended 2026-09-24 (FIG-3669), **not yet implemented**:
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
makes Restate the only effect engine and the SQL stores storage only. This ADR
specifies SQL-engine behaviour: ingress claims fenced by the session-execution
lease, and the in-process work driver on SQLite and PostgreSQL (A1); per-session
serialized execution becomes an engine obligation. Those passages stay as
written until the PR that deletes the code (FIG-3667, FIG-3668, or FIG-3600 for
the session lease) rewrites them.

## Context

Lash feeds turns from two durable queues. `pending_turn_inputs` holds host
input. `queued_work_batches` / `queued_work_items` hold process wakes, frame
handoffs and session commands. ADR 0010 made the split a rule: "It is not queued
work." Every lifecycle rule is therefore written twice: dedup, retention, cancel
disposition, ordering, claim bounds, settlement, affected-item records. The
second copy lags, and the lag has produced confirmed defects:

* **Order across the two tables is a timestamp guess.** The two `enqueue_seq`
  counters are incomparable, so the idle drain orders commands against inputs by
  `enqueued_at_ms`, ties to input, and claims wakes only when no input was
  claimed. A command enqueued before an input can apply after both the input and
  a wake.
* **A settled session command can run twice.** Settlement deletes the queued
  row, and enqueue dedup only sees live rows. A host that retries a config patch
  with the same key after settlement gets it applied again, possibly over newer
  values.
* **A conflicting replay is silently adopted** on the queued side (pinned by a
  conformance law), while ADR 0010 makes it an error on the input side.
* **A host-cancelled wake can come back** (FIG-3545): host cancel deletes the row
  without raising the wake redelivery floor.
* **An identical input retry is refused as a conflict** after its row was
  deferred (FIG-3544): dedup compares the row's *current* delivery, which every
  final commit and orphan repair rewrite from the addressed turn to the next
  turn.
* **The frame handoff lives in the queue** and inherits none of the protections
  it needs. After a crash it can be overtaken by input or commands, deleted by a
  host, stranded by a failed follow-on and later rendered as plain text in the
  wrong frame (FIG-3542), and its chain bound resets on recovery.
* **Cancel only knew one table.** Withheld wakes had no cancel disposition and no
  record, so a turn cancel completed them: a silent, floor-raising drop.

Every reference system with a durable per-entity queue keeps one row per item
with one lifecycle: Restate keeps one invocation-status row per invocation and
deleted its separate idempotency table; DBOS puts its queue columns on the
workflow row with partial indexes.

## Decision

**A session has one durable ingress: one table, one row per admitted item, one
order, one claim, one settlement planner, one cancel vocabulary. Kind-specific
behaviour is a property of the item's kind, never of a separate table or claim
type.**

### 1. The model

One table per SQL backend, `session_ingress` (SQLite) / `lash_session_ingress`
(PostgreSQL), and one `Vec` plus one counter in the in-memory store. One row is
one item. The batch/items split is deleted: every production producer admits one
item at a time, and the only multi-item constructor is a test. A *claim* is the
composition unit, and it already spans several rows.

| Column | Rule |
| --- | --- |
| `session_id`, `enqueue_seq` | One per-session sequence shared by every kind, taken under the session lock. |
| `lane` | `command \| turn`, fixed by kind (`session_command` → `command`; `input`, `process_wake` → `turn`). Order is `(lane, enqueue_seq)` (§4, §5). |
| `item_id` | Kind-derived. Input: the FIG-3513 acceptance-derived id. Wake: from the process and event sequence. Command: from its source key. |
| `kind` | `input \| process_wake \| session_command`. |
| `source_key` | One namespace, `UNIQUE(session_id, source_key)` over open and terminal rows alike. |
| `delivery` | The submitted `Delivery`: immutable intent, written once at admission and never rewritten (§5.1). |
| `submission_digest` | Written once at admission, never updated (§8). |
| `payload` | `Input(TurnInput) \| ProcessWake(ProcessWakeDelivery) \| SessionCommand(SessionCommand)`. The wake payload stays a copy of the process delivery. |
| `authority`, `merge_key` | Per-item data, nullable where a kind has none. They feed the drain policy and traces. Nothing authorizes on them and nothing gates a claim on them (§5). |
| `state` | `open \| accepted \| completed \| cancelled`. `held` stays a read projection, as ADR 0010 defined it. |
| `terminal_cause` | Closed, non-null exactly on terminal rows (§8). |
| claim columns | One set: `claim_id`, owner id and incarnation, `claim_token`, `claim_fencing_token`, `claim_session_lease_generation`, and the predecessor claim identity. Null on every terminal row. |
| `enqueued_at_ms`, `terminal_at_ms` | Informational and for claim-size bounds. Never an order key. |

`available_at_ms` is deleted: it has no non-test setter.

**One `Delivery`:** `Turn { turn_id, min_boundary } | AnyBoundary | NextTurn`.
Today's `TurnInputIngress::ActiveTurn` becomes `Turn`, `EarliestSafeBoundary`
becomes `AnyBoundary`, and `NextTurn` / `AfterCurrentTurnCommit` become
`NextTurn`. They are three variants, not two pairs collapsed into one.

**One claim type:** `IngressClaim = WorkClaim<IngressClaimData { mode, items }>`
with `ClaimMode::{ Idle, Checkpoint { turn_id, checkpoint }, Exact { item_ids }
}`.
*Amended (FIG-3540 S3, 2026-09-24):* `IngressClaim` is its own struct keyed by
drive epoch and admission id (ADR 0105 §2), with a clock-free replay-stable
token and no `WorkClaim` lease fields.
It replaces `TurnInputClaimMode` and `QueuedWorkClaimBoundary`.
An ingress drive is always a claimed drive: FIG-3532 removed the runtime
unclaimed drive, so there is no `Unclaimed` variant. `WithheldTerminalWork`
becomes one list of claims.

**One settlement/disposition planner** with one settlement regime, the claimed
one, and one disposition vocabulary `Complete | Drop | Defer`. Store-level
settlement of unclaimed rows (ADR 0069 §5) has no producer after FIG-3532 and
is deleted in the cutover (§14). The per-kind terminal side-write (the wake floor, §9)
is a property of the kind. `RuntimeCommit` carries one `completed_claims` list
and one `undelivered_claims` list (FIG-3531's field, generalized).

**One store trait:** `SessionIngressStore` replaces `TurnInputStore` and
`QueuedWorkStore`. Its enqueue answers
`Inserted | Existing | Conflict | WakeRewound`, and it owns claim, abandon,
cancel by id, source key or suffix, list (with `held`), vacuum and orphan repair.

#### Per-kind invariants

| | `input` | `process_wake` | `session_command` |
| --- | --- | --- | --- |
| Producer | Host, through durable acceptance (ADR 0069) | The process wake sender | `accept_session_command` |
| Delivery | `Turn`, `AnyBoundary` or `NextTurn` | `AnyBoundary` | `NextTurn` |
| Source key | Host-owned, or acceptance-derived (FIG-3513). A reserved system prefix is refused at admission. | `process:{pid}:event:{seq}:wake` | `command:{kind}:{key}` |
| Digest covers | Submitted delivery and input | The process fact only: process, event sequence, payload. Not the host-configured delivery policy. | The command |
| Lane | `turn` | `turn` | `command` |
| Claimed with | Inputs and wakes, as one FIFO prefix | Inputs and wakes, as one FIFO prefix | Nothing, except adjacent `ApplyConfigPatch` commands |
| Applied / delivered | At turn start or a checkpoint of the turn | At turn start or a checkpoint of the turn | Only at turn boundaries (§4) |
| Rendered as | Messages; the first input's options win | Wake causes; wakes carry no options | Never rendered |
| Terminal side-write | None | Floor raise, same transaction (§9) | None |
| Turn cancel | The accepted request's `undelivered` disposition, for items addressed to the cancelled turn | Always `Defer` (§10) | Never in a turn's claim |

### 2. What stays separate, and why

These are real boundaries, not leftovers of the split:

* **The process delivery outbox and sender allocation floors.** Process-owned,
  cascade with the process, and a different crash boundary from the session.
* **The wake redelivery fence** (`wake_redelivery_fences`). A side table that must
  survive `vacuum()`, because tombstones do not.
* **Turn-cancel arbitration** on the keyed-promise seam (ADR 0039). The ingress
  applies its settled evidence; it does not arbitrate.
* **Restate / native lane acquisition policy.** It paces who drives, not what is
  claimed.
* **`QueuedDrainPolicy`** as a host seam. It chooses how much of the legal prefix
  one claim takes (§5).

None of these is a second ingress.

### 3. The pending follow-on lives on the session head (F, amended)

A frame handoff is not an ingress item. The committed switch records the
obligation on the session head, where it is consumed exactly once:

```
PendingFollowOn {
    follow_on_turn_id, frame_id, task, options, chain_depth, attempts,
}
```

* **Where.** Its own nullable column on the head row, `pending_follow_on_json` on
  `session_head` / `lash_sessions`, beside `current_frame_node_id`. It is a
  column, not a `head_json` field, so a claim statement can evaluate the refusal
  below without decoding the whole config. It is in `RuntimeSessionState`, in
  `SessionHeadMeta`, and in the commit intent (§14).
* **Written** by the frame-switch commit, atomically with
  `current_frame_node_id = frame_id`. The switch commit's receipt records the
  value it wrote, so a replayed switch commit returns the same fact.
* **Turn id.** `follow_on_turn_id` is derived from the root turn of the logical
  run plus its position in the chain, so the root is recoverable from the id.
  The position is the physical-turn index, not a switch count: a FIG-3157
  checkpoint follow-on also advances the index, so a switch-count id could
  equal a turn already committed in the same run. A recovering run continues
  its index from the stored value.
* **Cleared** by the follow-on turn's terminal commit in the same head CAS,
  whatever the outcome (`Finished`, `Failed`, `Stopped(Cancelled)`, the
  frame-switch-limit error). If that turn switches again, the same CAS writes
  the next fact. Nothing else clears it; there is no delete path.
* **Precedence.** While the fact is set, every ingress claim is refused with the
  typed non-error `Blocked(FollowOnPending { follow_on_turn_id, attempts })`,
  except a `Checkpoint` claim whose `turn_id == follow_on_turn_id`. Drains see
  "blocked", never an error that a `?` would turn into a failed follow-on.
* **No other turn commits** (derived from F: host input that arrives before the
  follow-on runs is claimed after it commits). While the fact is set, a turn
  commit other than the follow-on's own is refused with `FollowOnPending`. Every
  direct turn claims its row since FIG-3532, so its initial claim meets the
  refusal above: its row stays queued in order and the drain answers it after
  the follow-on commits. The head-write refusal is the backstop for any commit
  that reaches the store anyway. *(FIG-3600: no direct turn exists after that
  cutover; the driver's claim meets the same refusal.)*
* **Head invariant.** Every head write checks
  `pending_follow_on.frame_id ∈ { None, current_frame_node_id }`. A commit that
  would move the frame while a follow-on is pending is refused, and
  `open_agent_frame` refuses with `FollowOnPending`. A stranded handoff is
  unrepresentable.
* **Recovery.** One check in the lease funnel: after acquiring the lease and
  refreshing the head, a set fact is driven as a logical run before any ingress
  claim. A Restate drive whose scope owns the chain (the root turn of
  `follow_on_turn_id` equals its scope) replays its own chain in order and never
  starts the fact from the lease
  funnel, so the positional journal cannot mismatch.
* **Recovery bound.** A drive that recovers the fact raises `attempts` by one
  in a fenced head write before the follow-on's first effect; on Restate this is
  the drive's first journaled step. The inline path never raises it. The bound
  is host policy: `max_follow_on_recoveries` lives on the same host durability
  object as the other claim bounds (§5; `QueuedWorkBatchingConfig` today), with
  a default of 3. When the raised value would exceed it, the drive does not run
  the follow-on. It commits the follow-on as a terminal `Failed` turn whose
  failure is the typed error `FollowOnRecoveryExhausted { follow_on_turn_id,
  attempts }`, with the task as its delivered input. That turn receipt is the
  follow-on's terminal record, and the same head CAS clears the fact. The count
  is never reset: no operator action, reopen or policy change resets it, and it
  lives and dies with the fact. Without a bound, a follow-on that crashes its
  process before committing would block the session forever.
* **Chain bound.** `chain_depth` carries `MAX_AGENT_FRAME_SWITCHES` across a
  crash instead of restarting at zero.
* **Cancel.** The follow-on is the cancelled logical turn's own continuation,
  not an undelivered item; `Defer`/`Drop` do not apply. A cancelled follow-on
  commits `Stopped(Cancelled)` with its task as delivered input, which clears the
  fact.
* **Fork.** A fork head starts with no pending follow-on; the source keeps its
  own.
* **Store-less sessions** carry the same field in memory, so the claimless
  in-memory branch and the `if claimed` guard are gone: one path.

*Implemented (FIG-3542), ahead of the table cutover.* The claims it refuses
are today's turn-input and queued-work claims: an idle claim meets
`QueuedWorkClaimRefusal::FollowOnPending` (or claims nothing), and a direct
turn's drive stays queued. The recovery bound is `max_follow_on_recoveries`
on `QueuedWorkBatchingConfig` and the exhaustion is
`TurnFailureCode::FollowOnRecoveryExhausted`. Recovery runs at the queued-drain
entry, the one place a redriven turn starts today
(`turn_loop/follow_on_recovery.rs`): the drain's own queued run resumes a
follow-on it owns, and a follow-on no run owns (a direct turn's switch) is
driven as its own logical run. The raise is a fenced head write that moves no
revision. S5 moves this call to drive admission (O6), where it becomes the
drive's first journaled step.

### 4. Session commands are a lane applied at turn boundaries

This section records the owner ruling that replaced G.

**Session commands are a class-level lane in the same table.** Order is
`(lane, enqueue_seq)` with `lane = command | turn`. Commands keep the one dedup,
tombstone, cancel and claim vocabulary of every other kind; only their order
and their application point differ.

**Commands apply only at turn boundaries:** after a logical run's final commit,
and at idle. At each boundary the driver first drains **all** open commands in
`enqueue_seq` order. Adjacent `ApplyConfigPatch` commands in the command lane
coalesce into one head commit (up to the existing 64-command cap per commit);
any other command takes a commit of its own. Only once the command lane is empty
does the driver claim turn-lane items, FIFO (§5).

Commands never apply:

* mid-turn;
* at a checkpoint — a checkpoint claim never looks at the command lane;
* between the physical turns of one logical run (D8). The claimless pre-turn
  command drain in `turn_loop/accept.rs` is deleted. A pending follow-on refuses
  every claim, command drains included (§3), so commands wait until the chain
  ends.

**Commands never block inputs, and inputs never block commands.** There is no
command barrier at idle or at checkpoints, and no exception for turn-addressed
items: a `Turn{t}` item is simply deliverable into its running turn at t's
checkpoints (§5.1).

**A running turn finishes under its config snapshot.** Config is snapshotted
when a turn starts. Every item delivered into that turn at a checkpoint, wakes
included, is seen under that snapshot, even if a command admitted meanwhile
will change the config at the next boundary.

**The accepted cost.** An input enqueued before a command may run under the
newer config: a command takes effect at the next turn boundary regardless of the
turn-lane items queued ahead of it. Config is a per-turn snapshot, not a
property of the queued item. A host that needs the old config for an input
waits for that input to run before submitting the command. The precedent is
Pekko's `ControlMessage`: a
class-level lane that jumps the ordinary queue but never interrupts the step in
progress, keeping FIFO within its own class. The strict FIFO barrier this
replaces is recorded under *Alternatives*.

`ClaimMode::Exact { item_ids }` stays as the one sanctioned out-of-order
selection within the turn lane, kept for host-selected drains
(`QueuedTurnBuilder` item ids). It is an operator or UI tool, never a producer
delivery class. It never selects a command. *(FIG-3600 deletes host-selected
drains: selecting items survives only as withdrawal or cancel (§10), so this
claim mode loses the producer it was kept for. See the amendment, A8.)*

A backend evaluates the turn-lane head inside the claim statement over a fixed
head set. It never uses a skip-locked scan that can pass a locked head row; copy
DBOS's partitioned-dequeue shape, not its `LIMIT … SKIP LOCKED` one.

### 5. Ordering and composition (E1, D14, D13)

**Within a lane, `enqueue_seq` is the only order. There is no kind priority
inside the turn lane**; the command lane is the one class-level exception (§4).
Enqueue order
equals per-session commit order: every producer, the commit path included, takes
the session lock before it takes the sequence number. Wall-clock time may bound
how much a claim takes (the maximum pending age); it never decides order.

#### 5.1 Turn addressing is immutable intent

A row's `Delivery::Turn { turn_id: T, min_boundary }` is stored once and never
rewritten. Its eligibility is derived, not stored:

* **While T is running**, the item is deliverable only into T, at a checkpoint
  whose boundary is at or after `min_boundary`.
* **Once T has ended** (its final commit is recorded, whatever the outcome), the
  item is treated as `NextTurn` by rule. The row is not updated; the claim
  statement derives the effective delivery from T's state.

**Admission validates the address.** At admission, under the session lock, a
`Delivery::Turn { turn_id: T, .. }` item is accepted only if T is the session's
running turn, or a turn of this session whose final commit is already recorded.
The second case behaves as `NextTurn` from the moment it is stored, by the rule
above. An address to a turn unknown to the session is refused with the typed
error `TurnAddressUnknown { turn_id }` and is never stored. Today admission does
not check the address at all (the host injection path enqueues
`active_turn(turn_id, AfterWork)` for any id). Without the check, a row
addressed to a turn that never runs would never become deliverable.

This deletes the re-defer rewrite that every final commit and orphan repair
perform today (§14). It also removes the root cause of FIG-3544: the stored
delivery can no longer drift from what was submitted, so an identical retry
always matches. The immutable submission digest (§8) stays as defence in depth.

#### 5.2 Composition

**One composition rule for idle and checkpoint claims of the turn lane alike.**
At idle it runs only after the command lane is drained (§4); at a checkpoint the
command lane is never consulted.

1. **Addressed items.** A checkpoint claim of turn t selects the open `Turn{t}`
   items whose `min_boundary` the checkpoint admits, in seq order, by address.
   They belong to the running turn; no earlier row holds them back. An idle
   claim has no addressed items: every `Turn{T}`
   item whose T has ended is `NextTurn` by rule (§5.1) and joins the prefix
   below at its own position.
2. **The FIFO prefix of unaddressed turn-lane items** (inputs and wakes).
   Starting at the lowest open unaddressed seq, the prefix extends in seq order
   and **stops, never skips**, at the first of (a `Turn{T}` item for an ended T
   counts as `NextTurn`; one for another running turn is not deliverable here):
   * a **delivery mismatch**: a row this claim mode cannot deliver. At idle both
     `AnyBoundary` and `NextTurn` are deliverable, so inputs and wakes share a
     prefix. At a checkpoint only `AnyBoundary` is, so a `NextTurn` row ends the
     prefix;
   * a per-kind cap (the FIG-3532 input bound, the wake bound), which is a stop
     point inside the one prefix, never a filter;
   * the one total bound;
   * the rendered-context reserve and maximum pending age of the claim policy.
3. **The host `QueuedDrainPolicy` chooses how much of that prefix to take.**
   Exact claims do not consult it.

`authority` and `merge_key` are per-item data: they reach the drain policy and
traces. They are not equality gates, and nothing authorizes on them. The two
per-kind caps and the wake policy fold into one claim policy on the host
durability object (`QueuedWorkBatchingConfig`), each cap host-configurable. The
turn-input cap defaults to 64 (FIG-3532). A backlog beyond a cap stays queued in
order, and its submitter sees the typed `Queued` outcome; nothing is dropped.
The literal `64` at the checkpoint claim is deleted.

**A queued direct turn succeeds.** A direct turn whose accepted row is queued
behind the claim bound returns the typed success outcome `Queued { ahead }`, not
an error (FIG-3532). The row stays admitted at its position, and the drain
answers it in arrival order, exactly once. Replay reports the same position.
*(FIG-3600: no caller runs a turn, so there is no caller turn to return
`Queued { ahead }`. A sent input waits at its position, and its handle's
outcome resolves when the driver answers it. See the amendment, A2.)*

A checkpoint's addressed items and its prefix are one claim and settle together.

### 6. Render order (D15)

Within one claim, committed history uses one fixed order on every path: host
input messages first, then wake causes, each in `enqueue_seq` order. FIFO
governs claiming; cross-kind order inside one turn carries no meaning. The order
matches what the model already sees, because every projector renders wakes as a
trailing turn-events block. Today the idle path commits wakes first and the
checkpoint path commits input first; both become input-then-wake.

### 7. Claims, deferral and redrive (D16, D17)

* Claims are fenced by the session-lease generation (ADR 0029) with per-row claim
  identity and no per-row expiry. Settlement checks claim identity, not only
  state.
* **`Defer` releases each row, never the claim as a unit.** A deferred row
  returns to `open` at its own `enqueue_seq`, its claim columns cleared, and the
  next claim recomposes from rows under §5. A multi-row claim is never replayed
  as a unit through a predecessor-claim path, which could move later members
  ahead of rows that became ready meanwhile.
* **An interrupted claim** (never released; its generation superseded) is the
  one case redriven from persisted per-row claim identity, because engine replay
  must re-derive the same turn (ADR 0069 §6). The drain policy is not consulted
  again.
* **`Complete` requires delivery evidence** (D11). The planner accepts `Complete`
  for an item only if the item is in the committing turn's rendered set. Every
  other exit of a claimed item goes through `Defer` or `Drop` with its
  affected-item record. The tombstone carries no settled-turn column; the
  commit receipt already links it.

### 8. Dedup, digest and tombstones (D1, D4, D5, D6, D12)

* **Immutable submission digest.** Admission writes `submission_digest` once,
  beside the immutable `delivery` (§5.1). A replay with the same source key
  compares digests only. Same digest → `Existing`, open or terminal. Different
  digest → a typed `Conflict` to the submitter, for every kind, never an
  untyped commit failure and never silent adoption. With delivery immutable the
  digest is defence in depth for FIG-3544, not its fix. It flips the
  conformance law that pinned silent adoption.
* **Wakes.** The wake digest covers the process fact only. The wake sender treats
  `Conflict` as a terminal discard with a non-blocking `ContentConflict` reason,
  so a conflict ends that delivery without stalling later wakes from the same
  process.
* **Reserved prefixes** are enforced per kind at admission: only the wake kind
  may use `process:…:wake`, only the command kind `command:…`. A host input
  using a system prefix is refused to the host before it can meet a wake.
* **Tombstones for every kind.** A terminal row stays until `vacuum()` removes it.
  Its fields: kind, source key, `enqueue_seq`, delivery, digest,
  `terminal_cause`, `terminal_at_ms`. An enqueue that meets a `cancelled`
  tombstone returns `Existing` and never reopens it.
* **Closed terminal cause.** `Delivered` (input or wake rendered by a committed
  turn), `Applied` (command), `StaleConfigRevision { base, head }`
  (`ApplyConfigPatch`, §12), `Cancelled(CancelReason)`.
* **Tombstones stay off the claim path.** On both SQL backends: a partial index
  over open rows; the unique source-key constraint covers tombstones (no SQLite
  `ON CONFLICT IGNORE`); a CHECK that terminal rows carry no claim and do carry a
  cause; an open-state predicate on every claim and head query.

### 9. The floor invariant (D3)

**Every terminal transition of a wake raises the wake redelivery floor to at
least its sequence in the same transaction**: `Complete`, `Drop`, host
withdrawal, and conflict discard. `Defer` never touches the floor. Vacuum may
delete a wake tombstone only at or below the floor. A redelivery at or below the
floor is absorbed. This fixes FIG-3545.

### 10. Cancel by author (E2, D12)

* **A turn cancel** (`Immediate` or `AfterStep`) applies the accepted request's
  `undelivered` disposition (`Defer` by default, or `Drop`) to the
  **host-authored items addressed to the
  cancelled turn** that it did not deliver, whether it held them or they were
  still open. `Defer` releases any claim and leaves the row as it is: T has now
  ended, so the item is `NextTurn` by rule (§5.1), at its own position. `Drop`
  tombstones it. Items not addressed to the cancelled turn are outside the
  disposition's scope; any it held are released at their positions. A
  `process_wake` it held is **always deferred**: the claim is released in the
  cancel commit, the row keeps its `enqueue_seq`, and the floor is unchanged. A
  wake's event text cannot be recovered by the model once dropped,
  so a collateral drop at every turn cancel would silently remove facts the
  model was about to see.
* **A host withdrawal** (by item id, source key or suffix) may cancel any
  undelivered item, a wake included. It writes a `cancelled` tombstone and, for a
  wake, raises the floor in the same transaction.
* **Every affected item is recorded**, deferred or dropped. The affected-item
  record carries `item_id, kind, source_key, enqueue_seq, disposition, reason,
  payload`, plus `fence_floor_after` for a dropped wake. `reason` is closed:
  `TurnCancelled { request_id, mode } | HostWithdrawn { selector }`. A `Drop`
  cannot be constructed without its record. The free-form
  `TurnCancelRequest.reason` stays host evidence on the request; it is not the
  record's reason.
* One cancel outcome enum serves every kind: today's five outcomes plus suffix
  cancel.

### 11. Recompose and render every claim

Every claimed item is rendered (D11 enforces it at settlement). A withheld claim
carried into a FIG-3157 follow-on is rendered whole by the one materializer; the
`.first()` asymmetry between what is rendered and what is settled is gone.

### 12. Commands are replay-safe by compare-and-set (D2)

Vacuum stays uniform for every kind: it has no horizon (ADR 0023), and command
tombstones are not exempt. The session-command completion marker is deleted.
Replay safety comes from the command instead:

* **`ApplyConfigPatch` carries the config revision it was written against and is
  refused if the head has moved.** The drain applies the patches of a coalesced
  claim in seq order, each against a running revision: a patch applies only if
  its `base_config_revision` equals the running value, and every applied patch
  advances it by exactly one. A refused patch settles as a `completed` tombstone
  with `StaleConfigRevision { base, head }`; its submitter receives a typed stale
  outcome (a new `SessionCommandSettlement` variant) and recomputes against the
  current head. The first application bumps the revision, so a replayed patch
  never re-applies: before `vacuum()` it meets its tombstone (`Existing`), and
  after `vacuum()` it is admitted as a new row and refused at drain by the same
  check.
* **`RefreshToolCatalog` is naturally idempotent.** It recomputes the tool
  surface from live sources and carries no revision, as its type already
  documents.

**Precondition finding: Lash has no config revision today, so this ADR
introduces one.**

* The head's `head_revision` (`SessionHeadMeta::head_revision`, a dedicated
  column) advances on **every** head commit, turn commits included. A patch
  submitted while a turn runs drains only at the next turn boundary (§4), after
  that turn's final commit, so a compare-and-set against `head_revision` would
  refuse almost every such patch. It is commit authority, not a config revision.
* `ApplyConfigPatch::schema_version` is the head wire generation
  (`SESSION_HEAD_META_SCHEMA_VERSION`), a codec discriminator.
* No protocol-level config revision exists; the only `expected_revision` in the
  protocol crates belongs to trigger subscriptions.

The new fields:

* **`PersistedSessionConfig::config_revision: u64`**, inside the head's config.
  It is `0` at session creation and advances by exactly one on every commit that
  applies a patch or otherwise changes the persisted config (the reopen seed
  commit when its reconciled config differs). Every other commit carries it
  unchanged. Because it is part of the config, it is part of the commit-intent
  preimage, and it is deterministic from the base head, so replay hashes are
  stable.
* **`ApplyConfigPatch::base_config_revision: u64`**, set at submission from the
  resident state's head config after the refresh the setter already performs.

This bumps `SESSION_HEAD_META_SCHEMA_VERSION` (11 → 12 at the time of writing)
and rides the §15 cutover.

### 13. Confirmations (D17), stated as laws

* One table, one row per item, payload inline; the side tables of §2 stay.
* Claims are fenced by lease generation, with per-row claim identity and no
  per-row expiry.
* Enqueue order equals per-session commit order.
* Order is by sequence only; the wall clock may bound claim size, never order.
* Items addressed to a finished turn are `NextTurn` by rule, at their own
  position (§5.1). Unlike today, no commit rewrites them.

### 14. Deletions

* Tables `pending_turn_inputs`, `queued_work_batches`, `queued_work_items` on
  every backend.
* `BatchId` (host-facing pins become item ids), `DeliveryPolicy`,
  `TurnInputIngress`, `TurnInputClaimMode`, `QueuedWorkClaimBoundary`,
  `QueuedWorkBatch` / `Item` / `BatchPayloads` / `Kind` / `Class`,
  `PendingSessionWorkOrdering`.
* Both family claim planners and both settlement planners, the paired
  claim-superseded errors, the `qwc` / `tic` claim-id dialects (one dialect
  remains), and the two-list `LogicalTurnClaims` / `TurnClaimSettlement` /
  `WithheldTerminalWork`.
* The timestamp arbitration between commands and inputs, and
  `session_command_precedes_turn_input`.
* `available_at_ms`.
* The session-command completion marker (`session_command_batch_completion_key`).
* The claimless pre-turn command drain in `turn_loop/accept.rs`.
* The command barrier, at idle and at checkpoints (D7), the rule that an exact
  claim must not jump it (D9), and the exception letting turn-addressed items
  pass it. Commands are a lane (§4). This also deletes
  `claim_leading_ready_session_command`'s head-only rule and the checkpoint SQL
  that returns nothing while a command is pending.
* The `frame_handoff` kind and `AgentFrameTask` payload, the "handoff options
  take the last" rule, `RuntimeCommit.enqueued_queue_batches` with its result
  field, both store enqueue loops, its commit-budget term, its preimage field and
  semantic-boundary entry, the seven-hop carrier from `logical_turn.rs` to the
  store, the inline exact-claim block for the handoff, and the claimless
  in-memory follow-on branch.
* The literal `64` checkpoint bound.
* Store-level settlement of unclaimed rows on all three stores, dead since
  FIG-3532 removed the runtime unclaimed drive: `TurnInputCompletion.claim:
  None`, `StoreError::UnclaimedTurnInputSettlementSuperseded` and its
  `turn_input_settlement_superseded` code, and the
  `unclaimed_turn_input_settlement_is_a_conditional_write` law. ADR 0069 §5
  carries the matching note.
* The re-defer rewrite of `Turn{t}` items to `NextTurn`, performed today by every
  final commit (`defer_to_next_turn`) and by orphan repair. Delivery is
  immutable, and an ended turn's items are `NextTurn` by rule (§5.1).

Public API breaks are accepted with no aliases (E4): `BatchId` becomes an item id
in `QueuedTurnBuilder`, `SessionCommandReceipt` and the queue events, and the
per-family error codes collapse into one code per condition. *(FIG-3600 deletes
`QueuedTurnBuilder` itself; see the amendment, A8.)*

### 15. Cutover

One wholehog cutover, one PR series ending in one cutover commit.

* **Reject-and-recreate** ([ADR 0081](0081-destructive-schema-changes-are-currently-reject-and-recreate.md)).
  SQLite `SCHEMA_VERSION` and the PostgreSQL schema version bump (74 and 115 at
  the time of writing); `CURRENT_SESSION_STATE_VERSION` moves 2 → 3 with
  `OLDEST_SUPPORTED_SESSION_STATE_VERSION = 3`, so an old session is refused at
  admission rather than converted; the head meta version bumps (§12); the
  commit-intent preimage changes (§3, one completed-claims list, no enqueued
  batches, the pending follow-on). Every bump goes through the durable-format
  registry (`scripts/versioned-surfaces.toml`, `lash::formats`) as recent
  cutovers do, and durable-read fixtures are regenerated.
* **No data migration and no aliases.** No converter step, no compatibility
  reader for the old tables, no old names re-exported.
* **In-flight Restate invocations are refused, not drained.** A journal written
  by the old binary carries acceptance and claim shapes the new binary does not
  replay. The cutover bumps the journaled acceptance and claim formats, and the
  new binary refuses an old-shape journal entry fail-closed with a typed error
  before any effect. There is no drain step and no compatibility replay. This
  follows the current clean-cutover policy (2026-09-24): while lash has no
  migration or drain path, a change that would otherwise need one bumps the
  gating version and refuses old durable state before any effect. The policy is
  temporary; the version gate is what a later migration or drain would key off.
* A cancelled turn must not settle withheld wakes as completed (FIG-3543, D10).
  Whatever the interim lanes ship, the cutover replaces it with §10.

### 16. Conformance laws

Every law runs on SQLite, PostgreSQL and the in-memory store, and the existing
turn-input and queued-work laws are re-expressed per kind against
`SessionIngressStore`. The crash matrix is re-run for the PostgreSQL wake
advisory lock on the merged table.

1. **Order.** `enqueue_seq` is strictly increasing per session and equals commit
   order across every producer.
2. **FIFO prefix.** An idle turn-lane claim is a prefix of the open unaddressed
   turn-lane rows; no claim skips a row it could not take.
3. **Stop points.** The prefix ends at the first delivery mismatch, kind cap,
   total bound or policy bound, and never continues past it, on either backend,
   including with a locked head row.
4. **Command lane.** At every turn boundary (after a logical run's final commit,
   and at idle), every open command applies in `enqueue_seq` order before any
   turn-lane claim; an open command never delays a turn-lane claim at a
   checkpoint, and an open input never delays a command at a boundary.
5. **Addressed items.** A `Turn{t}` item is claimable at t's admitting
   checkpoints regardless of earlier rows, and never into another turn while t
   runs. No write after admission changes a row's delivery; after t's final
   commit a `Turn{t}` item is claimed exactly where a `NextTurn` item at its
   position would be. Admission of a `Turn{T}` item succeeds only when T is the
   session's running turn or a turn whose final commit is recorded (then
   deliverable as `NextTurn` at once). An unknown T is refused with
   `TurnAddressUnknown`, and no row, tombstone or sequence number is written.
6. **Coalescing.** Adjacent config patches in the command lane share one head
   commit; any other command takes its own.
7. **Boundaries only.** No command applies mid-turn, at a checkpoint, or
   between the physical turns of one logical run; items delivered at a
   checkpoint are seen under the turn's start snapshot.
8. **Exact.** An exact claim selects only turn-lane items.
9. **Follow-on precedence.** While a pending follow-on is set, every claim other
   than the follow-on's checkpoint claims returns `Blocked(FollowOnPending)`, and
   every other turn commit is refused.
10. **Follow-on frame.** No head write leaves a pending follow-on whose frame is
    not current.
11. **Follow-on recovery.** A crash after the switch commit runs the follow-on
    first under `follow_on_turn_id`; the recovery past the configured bound
    (default 3) commits the typed `FollowOnRecoveryExhausted` terminal and clears
    the head; nothing resets the count; `chain_depth` survives recovery.
12. **Dedup.** Same key and digest → `Existing` while the row or its tombstone
    exists; different digest → typed `Conflict`, for every kind; an identical
    retry after its addressed turn ended or was cancelled with `Defer` is
    `Existing`.
13. **Prefixes.** A host input with a reserved prefix is refused at admission.
14. **Tombstones.** Every terminal row survives until `vacuum()`, is never
    claimable, carries no claim and carries a closed cause; a `cancelled`
    tombstone is never reopened by enqueue.
15. **Floor.** Every wake terminal raises the floor in its transaction; `Defer`
    does not; a redelivery at or below the floor is absorbed; vacuum never
    removes a wake tombstone above the floor.
16. **Cancel by author.** A turn cancel applies its disposition to the
    host-authored items addressed to the cancelled turn and to no other item;
    it defers every held wake (position kept, floor unchanged); every affected
    item is recorded; a host withdrawal of a wake tombstones it, raises the
    floor and records it.
17. **Delivery evidence.** `Complete` of an unrendered item is refused.
18. **Render order.** Input then wakes, each in seq order, identical on the idle
    and checkpoint paths.
19. **Recompose.** A deferred multi-row claim is recomposed row by row; a row
    ready before a deferred member is claimed first.
20. **Fencing.** ADR 0029's supersession law holds for the one claim type.
21. **Config compare-and-set.** A patch with a stale base settles as
    `StaleConfigRevision` and changes nothing; a patch replayed after `vacuum()`
    never applies twice; a coalesced group checks the running revision in seq
    order.
22. **Cutover refusal.** A pre-cutover store is refused at open, and a
    state-version-2 session is refused at admission.

## Amendment (FIG-3600, 2026-09-24): one `send()` ingress; the driver runs every turn

**Status.** Decided by Sam on FIG-3600, 2026-09-24. **Not yet implemented**:
FIG-3600 builds it inside the FIG-3540 cutover series, after FIG-3585, so the
cutover changes two stores, not three. It is one wholehog cutover: no aliases,
no compatibility path for caller-driven turns, and no convenience wrapper that
runs a turn inline. The evidence, including the design that was not taken, is
in `/workspace/notes/lash/fig3573-arc/input-lifecycle/`. It amends ADR 0045,
ADR 0069 (§7 is superseded on landing) and the `CONTEXT.md` glossary; each
carries a note pointing here.

### A1. The ingress is the only way a turn starts

**A turn starts only from the session's ingress, and only the backend's work
driver runs it.** Direct and queued turns collapse into one path,
`session.send(input)`, with durable acceptance
([ADR 0069](0069-durable-acceptance-is-the-sole-turn-ingress.md)).

* **The driver always runs the turn, even when the queue is empty.** If the
  session is idle with an empty queue, the driver claims the input at once;
  that is the old "direct" behaviour. Otherwise the input queues on the lanes
  (§4, §5). The caller never runs a turn inside its own call.
* **The backend carries the work driver.** The in-process driver on SQLite and
  PostgreSQL, and the engine-backed driver on Restate, are part of the
  `Backend` ([ADR 0102](0102-zero-infra-is-a-sqlite-in-memory-backend.md)). This
  settles FIG-3581's open question about `process_work` / `with_queued_work`.
* **Delivery modes replace steer and follow-up.** `NextTurn` is the default,
  `AnyBoundary` steers into a running turn, and `Turn { id }` addresses one
  turn (§5.1).
* **"Now, despite the queue"** means the host withdraws or cancels the items
  ahead (§10). There is no fast path and no priority.

### A2. The caller observes through a handle

`send` returns a handle:

* `handle.events()` subscribes to the session observation stream from a
  cursor;
* `handle.outcome()` resolves to `Answered | Failed | Cancelled | Parked`.

Cancel goes through `session.cancel(input or turn id)`: a host withdrawal while
the input is queued (§10), and a durable turn cancel
([ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md))
once it runs. It replaces `cancel(CancellationToken)`. Activity sinks,
`stream_to` and the effect sinks become `handle.events()`. Dropping a handle
stops nothing: abandonment is expressed by cancel, never by silence (ADR 0069
§3).

### A3. Continuation belongs to the substrate

Continuation is the substrate's for every turn
([ADR 0045](0045-services-are-stateless-substrates-own-continuation.md)),
because no caller-driven turn exists.

* A deterministic failure is recorded as a failed turn (FIG-3575), and the
  outcome is `Failed`.
* Live faults and crashes are re-driven by the substrate under its own policy:
  the engine's retry on Restate, the worker's retry budget on SQLite and
  PostgreSQL. A re-drive keeps the same turn id and replays the journal.
* Lash never settles an attempt on anyone's behalf. With no caller-owned aborted
  turn, nothing binds an input to an aborted turn (ADR 0069 §7).

### A4. Parked is a generic state of a driver-run turn

A parked turn is not specific to code cells. Either of two causes parks a turn
the driver runs:

* the substrate's re-drive budget is exhausted;
* a replay divergence (FIG-3586).

A parked turn is neither failed nor retried live. It is visible in drain status,
and `handle.outcome()` resolves to `Parked`. The operator and host verbs are
**re-drive** (same turn id, replaying the journal; after a divergence, on a
build that matches it), **cancel** (`session.cancel`, ADR 0039) and **fork**.

### A5. Session commands, and the session model as durable config

**Session commands keep the command-lane semantics of §4**, including
`SetModel`. They are applied at turn boundaries, and every pending command is
applied before the turn-lane claim. The driver takes the per-turn config
snapshot at turn start, **after** that drain.

**The session model is durable session config:** a route
`{provider, model}` plus settings such as the thinking level. It is never a live
handle.

* **Changed by command.** It changes through a session command, a config patch
  on the command lane (§4, §12), and the change is recorded in history.
  `send(SetModel)` followed by `send(input)` therefore runs that input on the new
  model, deterministically. This is pi's `model_change`.
* **Resolved at turn start.** The driver snapshots the session config and
  resolves the route by name through the backend's provider resolver. A
  re-drive resolves the same route. There is one resolution point per
  execution.
* **Validated twice.** When the command is sent, a bad route is refused at once
  with a typed refusal and nothing is queued. When it is applied, at a later
  time and possibly on another worker, it is checked again. The typed refusals
  are `ProviderRouteUnknown` and `ProviderCredentialsMissing`.
* **A refusal at apply** leaves the session model unchanged, and the turn queued
  behind the command fails with that typed refusal. Nothing runs on a model the
  host did not intend; there is no silent substitution.
* **No per-turn override.** `TurnBuilder::provider(ProviderHandle)` is deleted.

**The *Session Model* rule changes.** It used to say that the host supplies the
model at every open and that stored state is never authoritative. The host now
sets the model by command, the session state is durable, and the driver reads
it. A worker reopening a session after a crash has no host to ask.

### A6. Everything on a sent input is durable data

* Already durable: the prompt template, contributions, slots and layer (all in
  `turn_context`), and `protocol_turn_options`.
* Deleted: the live `protocol_extension` and `live_plugin_inputs`. Durable hosts
  already refuse both; `protocol_turn_options` and persisted plugin state are
  their durable forms.

### A7. No injected prompts

Lash never writes model-visible markers or feedback because something failed or
crashed. The model only ever sees real committed history.

### A8. Deleted in the cutover

* **Caller-driven turns:** `TurnBuilder::{run, run_with_effects, stream,
  stream_to, stream_to_with_effects, *_with_scope, collect*}`, `AdvancedTurn`,
  and the drive half of the direct-turn path
  (`crates/lash-core/src/runtime/turn_loop/accept.rs`).
* **Caller-driven drains:** `QueuedTurnBuilder`, the selected-drain builder, and
  `drain_id` / `batch_ids` as caller-run drains. Selecting items survives only
  as withdrawal or cancel (§10), so `ClaimMode::Exact` (§4) and law 8 lose the
  host-selected drain they were kept for.
* **FIG-3589's surface:** `claim_bound_turn_id`, `claim_bound_receipt_input_id`,
  `PendingTurnInputReadStatus::TurnBound`,
  `PendingTurnInputCancelOutcome::TurnBound`, and the receipt re-drive docs in
  `crates/lash/src/error.rs` and ADR 0069 §7. With no caller-owned aborted turns,
  nothing needs them.
* **Old gates and live inputs:** FIG-3416's durable-admission gate
  (`ensure_durable_effect_input`), the live `protocol_extension`,
  `live_plugin_inputs`, and per-turn provider plumbing.
* **Old vocabulary:** "Queued Turn" versus direct turn, in `CONTEXT.md`, ADR 0069
  and this ADR.

Every `session.turn(..).run()` / `queued_turn()` call site moves, repo-wide and
in the same cutover, to `send` plus `outcome()` or `events()`.

### A9. Not adopted

* **FIG-3597 design (b), auto-cancel and hide.** It cancelled an aborted turn's
  bound input and hid it at the next commit. That hides a loss from the host
  and already-run effects from the model. With no caller-owned aborted turn,
  there is no bound input left to hide.
* **The pico3-style unanswered-marker lifecycle** (the rescoped FIG-3597 draft).
  It settled an unanswered input into history with a model-visible abort marker.
  The marker is an injected prompt, which A7 forbids. Its content had no durable
  source for a turn's tool calls either. The substrate re-drives the turn or
  parks it instead.
* **A boundary commit triggered by relinquishment** (the lead's tie-break in
  the combined critique). It was a separate journaled commit that settled a
  relinquished attempt before the next turn claimed. It existed only because
  the caller owned an aborted direct turn's continuation. Now the substrate
  owns it, and an exhausted budget parks the turn for an operator or host
  decision (A4); nothing settles it on anyone's behalf.
* **`abandon(receipt)`.** It was a verb for a caller-owned aborted turn.
  `session.cancel(input or turn id)` covers withdrawal, turn cancel and the
  cancel of a parked turn: one verb.

The peer evidence is pi `a8ed4977` (one `send()`, with steer and follow-up when
busy; the model as a session setting with `model_change` entries), codex
`7db578f`, openai-agents `32edd3c` / js `a0b1c6f`, and langgraph `1211af4`.

## Alternatives considered

* **Keep two tables (ADR 0010).** Rejected. Every defect in *Context* is a rule
  implemented twice with the second copy lagging.
* **"Structural merge only", preserving every current arbitration.** Rejected.
  The timestamp arbitration exists only because two counters are incomparable;
  keeping it would re-implement an artefact of the split inside the merged model.
* **Keep the batch/items shape.** Rejected: it preserves a shape no production
  producer uses. A claim already composes several rows.
* **A `dedup_domain` column.** Rejected: a second key for a collision with no
  producer. Reserved prefixes per kind cover it.
* **Silent adoption of a replay with changed content.** All five references do
  it. Rejected; lash fails closed, and the immutable digest keeps that sound.
* **Kind priority: host input before wakes.** Rejected (E1). It reproduces DBOS's
  priority starvation, and today's version already inverts command order. If
  reply latency ever outranks uniformity, the only admissible form is Pekko's
  class-level order with `enqueue_seq` as a stable secondary key.
* **Commands as a strict FIFO barrier in one sequence** (the earlier G ruling,
  with D7 and D9). A command at seq s blocked every unaddressed claim with a
  higher seq, at idle and at checkpoints; turn-addressed items needed an
  exception to pass it, and exact claims needed a rule not to jump it.
  Replaced by the owner for two reasons. First, command latency: a config patch
  waited behind the whole queued backlog, and its settlement waiter mostly
  returned `Pending`. Second, barrier complexity: a stop rule in every claim
  statement on both backends, a checkpoint rule, an addressed-item exception
  and an exact-claim refusal, all for an ordering guarantee — "queued work runs
  under the config it was queued under" — that config snapshots per turn do not
  need.
* **A per-item "ahead of queued inputs" flag, a second table, or a time key for
  commands.** Rejected. A per-item flag would have to pull every earlier command
  along, a second table re-creates the split this ADR removes, and a time key
  is the arbitration being deleted. The class-level lane (§4) is the one
  admissible form.
* **The frame handoff as an `Exact`-only queue kind.** Rejected (F). The handoff
  uses almost none of the queue lifecycle, and that lifecycle is where every
  handoff defect comes from. A head fact makes the stranded handoff
  unrepresentable.
* **The pending follow-on inside `head_json`.** Rejected: no claim transaction
  reads the head config today, and the refusal must live in the claim statement.
* **An unbounded follow-on retry.** Rejected: the block is session-wide, so an
  unbounded retry of a crashing follow-on blocks the session forever.
* **Exempt command tombstones from vacuum, or keep a vacuum-proof marker** (the
  prospect's D2). Rejected by the owner ruling. Either keeps command evidence
  forever and makes vacuum non-uniform; compare-and-set makes the command itself
  replay-safe.
* **Compare-and-set against `head_revision`.** Rejected: it advances on every
  turn commit and would refuse nearly every patch queued behind a running turn.
* **Authority and merge key as composition gates** (the prospect's D14 reading).
  Rejected by the owner ruling: under that rule inputs and wakes would never
  share an idle claim, because inputs carry no authority, and nothing authorizes
  on either field.
* **"NextTurn rows block nothing at a checkpoint"** (the prospect's G note).
  Rejected: the owner ruling makes a delivery mismatch a stop point for the
  unaddressed prefix on both paths. Only addressed `Turn{t}` items are selected
  out of FIFO order.
* **Uniform `Drop` on wakes at turn cancel** (Fable E.2). Rejected (E2): the
  model cannot recover a dropped wake's event text.
* **Keep re-deferring addressed items on every final commit.** Rejected (owner
  ruling on turn addressing): a stored delivery that commits rewrite is the
  root cause of FIG-3544, and "ended turn → next turn" is derivable from the
  turn's own final commit.
* **Migrate existing rows.** Rejected: reject-and-recreate is the accepted
  cutover (ADR 0081), and a converter would be the shim this ADR removes.
* **Aliases for renamed public types.** Rejected (E4): a shim.

## Consequences

* Every lifecycle rule (dedup, retention, cancel, ordering, claim bounds,
  settlement) is written once, and a new kind inherits all of it.
* ADR 0069's "sole turn ingress" becomes literal: the session ingress is the
  acceptance row class for host, process and command admissions, and no commit
  writes ingress rows.
* Turn-lane order becomes strict FIFO. A user message waits behind wake turns
  enqueued before it, bounded by the drain policy. With the shipped
  `OneAtATime` policy each turn is one row anyway.
* A config patch submitted while a turn runs applies right after that logical
  run's final commit, ahead of every queued input and wake. Queued work
  enqueued before the patch may therefore run under the newer config; this is
  the accepted cost of §4.
* Hosts that retried a config patch against stale resident state now receive a
  typed stale outcome instead of a silent overwrite.
* Wake tombstones add volume until `vacuum()`; that is a vacuum-scheduling note
  under ADR 0067, not a second retention model.
* One cutover invalidates every existing session and store, and in-flight Restate
  invocations must drain first.
* Two live bugs (FIG-3544, FIG-3545) have standalone fixes now. The floor rule
  carries into the cutover unchanged. For FIG-3544 the cutover removes the root
  cause (immutable delivery, §5.1) and keeps the digest as defence in depth.

## Links

* FIG-3540 arc and owner rulings; children FIG-3541 (command double-apply,
  fixed by §12), FIG-3542 (frame handoff as a queue row, fixed by §3), FIG-3543
  (cancel completes withheld wakes, fixed by §10), FIG-3544, FIG-3545.
* Prospect report `/workspace/notes/lash/prospect-ingress-2026-09-23.md` (D1–D17,
  answers to F, G, E1, E2).
* [ADR 0023](0023-retention-stays-a-parameterized-host-lever.md) — vacuum has no
  horizon.
* [ADR 0081](0081-destructive-schema-changes-are-currently-reject-and-recreate.md)
  — the cutover posture.
