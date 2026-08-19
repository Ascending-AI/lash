# Concurrent settlement is a durable group at the effect-host seam

## Status

Accepted. Amended 2026-08-19 (FIG-1578): a group carries envelopes and nothing
else, and what runs a child is the host's registered `GroupExecutors` resolver
rather than a caller-supplied executor vec paired with the group.

## Context

A tool batch has been one atomic effect: `call_tool_batch` built a single
`RuntimeEffectCommand::ToolBatch` envelope and made exactly one `execute_effect`
call for the whole batch, so the VM stayed inside that one host effect until
every leaf settled. Children were journaled individually, but the *order in
which they settled* was not: it was derived from a `FuturesUnordered` yield
order in process, as `tool_dispatch/scheduling.rs` conceded in its own comment
("the only place the settlement order of a batch exists"), and rode out as a
field on the batch result, journaled only when the complete batch record sealed.

Three consequences followed from that one shape.

A crash after k of n leaves lost the ordering of those k. Redrive re-raced and
could produce a different permutation, so an aggregate selecting its first
*settled* rejection could select a different one after a crash. The field
already failed closed rather than repairing a malformed order into a plausible
one (ADR 0062), so what was missing was never validation — it was durability.

`Promise.race` and `Promise.any` could not be expressed at all. A batch's
*return* is its all-settled point, so first-settlement resume has no seam to
land on, and synthesizing race by reporting the first entry of the settled order
would still wait for every leaf — a `race([tool(), sleep(t)])` that never fires
early is wrong in exactly the case people write it for. Both combinators
therefore rejected with a named diagnostic.

And Restate never ran batches concurrently at all. `supports_concurrent_effects()`
is `false` there because the Rust SDK requires `ctx.run` closures to be awaited
immediately, so the batch path took a serial branch and reported input order.
That report was honest — execution really was serial — but it meant the same
program could select a different rejection on Restate than on any other tier.

The standing doctrine constrains the fix. Durability gaps are closed by
extending the effect-host contract, which Restate and Temporal implement; lash
never builds its own effect journal (ADR 0012). Both target engines already own
durable child completion and replay-deterministic selection natively — Temporal
as per-completion history events, Restate as durable futures with per-completion
journal entries — so the missing piece was contract wording, not machinery.

One caveat on that reading, because it understates what the contract asks of
Restate. The normative cursor rule below requires serving rank `consumed + 1`
*idempotently after a fresh invocation*: the handle is the sole cursor of record,
so a host must be able to re-read the settled children of a group by rank rather
than consume the next completion as it arrives. That is a random-access read over
settled children, strictly stronger than "durable futures with per-completion
journal entries", which gives replay-deterministic *arrival* order and nothing
addressable. The Restate host spec must account for it — a group's settled ranks
have to be recoverable in a new invocation that holds none of the original
futures — and it is the reason the cursor lives on the caller's handle rather
than in a host-side position.

## Decision

**Concurrent settlement is a structured, durable group at the effect-host seam.**
The contract gains one capability and three methods on `RuntimeEffectController`,
beside `supports_concurrent_effects` and `execute_effect` — not on
`AwaitEventResolver`, and not on `EffectHost`, whose levers are deployment-level
while group state is scope-level.

The contract says **durable child completion and first-settlement wake** and
names no engine primitive. A conforming host must be able to (a) start N
children as independently durable units, (b) report which settled first as a
durable fact, and (c) let the losers finish under its own ownership after the
caller has moved on.

- `supports_effect_groups()` — checked once at **deployment validation**, not
  per call. The group path is the only tool-batch path, so a controller
  answering `false` has no batch path at all, and a host wiring one should learn
  that at startup rather than mid-turn on its first `Promise.all`. It gates
  admission, not dispatch. A host may answer `true` **only if it has a registered
  `GroupExecutors` resolver**, because that resolver is where the `'static`
  executors come from: a child must be able to outlive its caller to honor
  `RunToCompletion`, and the borrow-scoped executor `execute_effect` takes carries
  the one lifetime this contract exists to break. The two capabilities are one
  question and must not drift apart, so on the in-process tiers this answer *is*
  "a resolver is registered" rather than a constant.
- `open_effect_group(group)` — returns once the group is durably recorded, **not**
  when a child settles. Its one parameter is the `RuntimeEffectGroup` itself:
  **envelopes, and nothing else.** What code runs a child is answered by the
  host's registered `GroupExecutors` resolver, which maps an envelope to a
  `'static` executor. **That resolver is the contract's only executor-resolution
  seam, and it is normative**: the open, a retry after a claim expires, and the
  loser drain all reach for a child's runner through the same registered object,
  so one host has one answer to what runs a journaled child.

  It has to be the host's, not the caller's, because three of the four paths that
  need a child's runner happen where no caller is in scope — a retry, the drain,
  and a resuming process reopening a group it never opened — and a fourth,
  `child-as-invocation` on an engine tier, cannot carry a closure across a fresh
  handler execution at all. A caller-supplied vec can answer only the first path,
  which is how the pairing type this ADR originally made normative
  (`CheckedEffectGroup`) came to be retired: arity was the wrong thing to make
  unrepresentable, since the question is routing, not alignment.

  **A child with no runner is a routing fact, not an outcome.** A resolver that
  answers `None` for a child means the deployment cannot run it — not that the
  child failed — so no terminal is ever synthesized from a miss. In-process hosts
  resolve **all N children before journaling anything**: any `None` refuses the
  whole open with a typed group-shape error and writes nothing, so a group is
  never half-opened around a child that will never settle. A journaled group can
  still meet a host that cannot run one of its children — that is a deployment
  change, not an open — and the drain reports it as `NoExecutor`, leaving the
  group unreclaimable and visible rather than inventing a terminal for it.

  Each executor is single-execution: `execute` consumes it, so a host that
  retries a child resolves a fresh one through the same registered resolver. A
  reopen must be **fenced on group shape** — a recorded group whose child count
  or wake rule differs from the group passed in is refused, because a shrunk
  child vec under one key silently renumbers every rank above the truncation and
  the per-child hash fence cannot see it.
- `await_next_settlement(handle, cancel)` — delivers settlements one at a time.
  The handle is taken by `&mut` and is **the sole cursor of record**: the host
  advances it on exactly the settlements it returns and keeps no per-caller
  consumption state, so awaiting rank `consumed + 1` twice — once before a crash
  and once after — yields the same settlement. It follows that a host must not
  implement the await as "take the next journal entry", which would advance
  regardless of the cursor. On reopen the **caller's** cursor wins: a host knows
  how many children settled, only the caller knows how many it consumed, so open
  returns `consumed = 0` and a restored frame supplies the cursor it saved.
  Cancellation leaves cursor and durable rank untouched; exhaustion is the
  caller's arithmetic, not a host round trip. Both sides of the cursor are fenced,
  not just the read: a handle takes its child count from the group so the two
  cannot disagree, and advancing past the last child is **refused rather than
  clamped**, so a host serving a rank it cannot have fails at the slip instead of
  writing a continuation that turns out to be unresumable when it is loaded.
- `close_effect_group(handle, disposition)` — releases the caller's interest, and
  is **idempotent**: the handle is deserializable, so a crash between a
  successful close and the continuation commit means a replayed frame closes the
  same group again by construction. `disposition` may only **narrow** the one the
  group declared at open (see "Loser disposition is declared at open").

The three group methods keep defaults that error loudly with
`RuntimeErrorCode::EffectGroupUnsupported`, following
`cancel_await_events_for_session`: an out-of-tree controller that has not
implemented groups fails closed with a named error rather than mis-executing a
batch.

### The settlement obligation

Stated engine-portably, because it is the whole of the replay argument:

> **Settlement `n` of a group is a durable fact, and every replay observes the
> same child at position `n`.**

A host must not re-derive position `n` by racing live children once `n` has been
decided. How the fact is stored is the host's business — a SQL row, a Restate
journal entry, a Temporal history event. This is a contract obligation, not an
optimisation.

Settlements are served by **rank** — the child holding the `(consumed + 1)`-th
smallest sequence — and never by literal sequence equality. Sequences are
monotonic and unique within a group but deliberately **not gapless**: journal
retirement and rolled-back finalizes both remove values. Rank is gap-immune
because it counts recorded children rather than counting up through integers.

Three properties make rank stable, and all three are required:

1. **The counter is strictly monotonic.** Every allocation returns a value
   greater than every value previously returned for that group, so no child can
   later be assigned a sequence below an already-allocated one.
2. **The recorded set is append-only below any consumed rank.** A child settling
   after the caller consumed rank j necessarily draws a sequence above all j
   already-allocated values, so it appends above and never inserts below.
3. **Retirement is group-atomic.** A group retires whole or not at all. This is
   the property that closes the remaining hole: a *deletion* below a consumed
   rank would shift ranks even though allocation never does.

### Wake policy is journaled identity

`GroupWakePolicy` has exactly three variants: `First` (`Promise.race`, and the
signal/deadline select), `FirstSuccess` (`Promise.any`), and `All`
(`Promise.all` **and** `Promise.allSettled`).

`all` and `allSettled` share a variant because they ask the host for exactly the
same thing — deliver settlements in durable rank order, keep the rest running —
and differ only in how far the *caller* consumes: `all` stops at its first
rejection, `allSettled` consumes everything. That early exit is a caller-side
loop decision, never a host obligation. A fourth variant would make journaled
identity pin a distinction no host acts on.

The consequence is worth stating because it is strictly stronger than the
position ADR 0062 had to defend: `all`'s first-settled rejection becomes a
*consequence of consuming settlements in durable rank order* rather than a
permutation the host must be trusted to report. `settlement_order` survives as
an observability projection, not as the mechanism `all`'s correctness rests on.

Because the wake rule is identity, all three variants ship together. Adding a
wake policy after the fact would change the identity of groups already recorded.

### Normative: finalize ordering for a grouped child

One transaction, in this order. Getting it backwards reintroduces corruption in
a form no uniqueness constraint catches.

1. Perform the existing fenced `UPDATE` on the child's replay row — the guarded
   write that already exists, matching all five fence columns.
2. **If its rowcount is 0: roll back and report the no-op.** No counter bump.
   The fence moved; this driver no longer owns the child.
3. Only if its rowcount is exactly 1: bump the group's counter atomically and
   write the returned value as this child's settlement sequence.
4. Commit.

An implementation that bumps first, or bumps unconditionally and commits while
reporting the no-op observation, lets a **taken-over driver permanently advance
another live group's counter**. That is not hypothetical: `finalize`'s contract
says to report `false` when the guarded write matched no row, and the sibling
`claim` explicitly blesses a committed transaction that merely reports an
observation. So a bump-then-report-false implementation would look
contract-conformant while corrupting a group it does not own — and a uniqueness
constraint on `(group, sequence)` does **not** catch it, because the burned
number is never written to any child row.

Such a constraint is kept anyway, as belt-and-braces for the case it does cover:
a future regression to a read-then-max allocator seating two children at one
position fails closed on a constraint violation instead of silently succeeding.
That is the same fail-closed-over-repair posture `settlement_order` already
takes.

### Normative: lock order is child row, then group row

`open_effect_group` writes the group's record before its children, while
finalize takes the child before the group. That asymmetry is an ABBA deadlock if
`open` is ever implemented as one transaction spanning both.

> **A group's record is created and committed in its own transaction, before any
> child claim is issued.** `open_effect_group` therefore never holds a group lock
> while acquiring a child lock, and the global lock order for any transaction
> touching both is **child row → group row**, without exception.

The failure this prevents is a detected abort rather than corruption, but it
would surface as intermittent group-open failures under concurrency — an
expensive thing to diagnose for a constraint that costs one sentence to state.

### Normative: ungrouped effects stay hash-identical

Group membership rides `RuntimeEffectEnvelope` as an **optional field, omitted
when absent**, so an ungrouped effect's canonical encoding — and therefore its
recorded `envelope_hash` — is byte-identical to what it was before groups
existed.

This is a blocking constraint, not a style preference. SQLite is
reject-and-recreate on its effect-schema version, but Postgres is not: a live
replay table survives the upgrade with all its recorded hashes. An unconditional
encoding change would invalidate every one of them, so every in-flight effect at
upgrade — not just grouped ones, *all* of them — would come back as a replay
mismatch and fail closed. The blast radius would be the entire deployment's
in-flight work, caused by a field those effects do not even use.

Folding the wake rule into each child's hash is also what makes "replay cannot
silently change the wake rule" backed rather than asserted: the existing
envelope-hash fence refuses a replay whose wake rule drifted. It is the only
mechanism available on engine tiers that keep no group record at all.

The two rules divide the space cleanly. Hash stability covers effects whose
encoding is unchanged. In-flight *batches*, whose shape genuinely changes from
one journaled entry to n children, are covered by the other standing rule: per
ADR 0055 there is no migration decoder, so deployments drain before the format
bump.

### Normative: a group's copies are made to agree by construction

Every durability claim here reduces to three copies agreeing: the group key (on
the group row and in each child's membership), the wake rule and disposition (the
same two homes), and each child's position (its index and its membership's
`position`). Disagreement's only symptom is a `ReplayMismatch` in someone's
production journal, so it is made unrepresentable rather than documented: a group
has exactly one constructor, which stamps unstamped children from their own index
and refuses any child that disagrees with the group it claims — a foreign key, a
permuted position, a drifted wake rule, or a drifted disposition. Hosts therefore
never recover group identity from `children[0]`.

Empty groups are refused. `Promise.all([])` resolves immediately with `[]` and
`Promise.race([])` never settles; neither has a child to journal, so neither is a
durable fact and neither reaches this seam — the dialect resolves the first
locally, and the second is a never-settling program that must not become an
unbounded durable await.

A grouped child that reaches a dispatch path with no slot for its membership is a
typed refusal, never a silent strip. Restate's timer, await-event, and process
executions record no canonical envelope at all, so on that tier those commands
have no hash to fold a wake rule into; dropping the membership there would remove
the only fence the engine tiers have. That the two arms concerned are `Sleep` and
`AwaitEvent` — precisely the children of the deadline/signal select — is the
reason this is a refusal rather than a note: the first real consumer lands on
them, and the Restate layer must convert the refusal into real child invocations
rather than discover it.

### Group identity carries an occurrence discriminator

A group's key is `{scope_id}:group:{batch_id}:{occurrence}`.

The obvious derivation is unsafe. A batch id is a *content hash* of its calls,
so two textually identical `Promise.race([a(), b()])` calls in one protocol
iteration hash identically and collide — harmless while a batch was one sealed
effect, **fatal** once siblings share a group counter, because the second
group's children would allocate from the first group's record. The
`protocol_iteration` component does not discriminate two calls *within* one
iteration, which is precisely the colliding case.

The occurrence ordinal comes from the VM's deterministic effect sequence, so it
is replay-stable, and it **rides the VM continuation** — a counter living only
in live VM memory would restart after a snapshot, so two identical `race` calls
straddling a park would both derive occurrence 0 and collide exactly as the
content hash does. ADR 0025 already enumerates occurrence counters among the
continuation's contents.

### Loser disposition is declared at open

**The disposition is a per-group durable fact, declared when the group is opened
and journaled with the group row — not an argument chosen at close.** It is
statically known at open, so nothing is lost by requiring it there, and leaving
it at close was a real hole: a caller that crashed after `open_effect_group` and
before `close_effect_group` left the host no record of which disposition applied,
so the group-drain path below had to invent one. Inventing meant running *every*
abandoned group's losers to completion, silently downgrading a deadline arm's
`Cancel` to `RunToCompletion` on exactly the failure path this ADR exists for —
and each backend would have invented differently (Restate: the engine owns the
losers and never cancels them; SQL: the drain completes them; in-memory: process
death cancels them implicitly), which is the ADR 0062 divergence shape.

It is the same class of fact as the wake rule, and it is treated the same way: it
is folded into every child's envelope hash as well as the group row, so a replay
under a drifted disposition is refused on engine tiers that keep no group row.
Shipping it late was impossible for the same reason a fourth wake policy is —
it would change the identity of groups already recorded.

`close_effect_group` may therefore only **narrow**: a declared
`RunToCompletion` may be tightened to `Cancel` by a caller that has learned it no
longer wants the losers, but a declared `Cancel` may not be widened back, and the
attempt is a typed refusal. Widening would make the losers' fate depend on
whether the caller happened to reach its close at all, which is precisely the
divergence declaring at open removes.

Phase-1 consumers fix the disposition at the combinator: `all` and `allSettled`
declare `RunToCompletion`; `race` and `any` declare per the ratified race
semantics below.

`LoserDisposition::RunToCompletion` is the default for `race`/`any` because it
is what ECMA-262 specifies: a losing promise keeps running and its side effects
still happen. Cancel-always would be simpler — no background ownership, no
redrive question, no unbounded loser tail — but it would be a silent divergence
in exactly the family ADR 0062 forbids.

`LoserDisposition::Cancel` survives as the *correct* semantics for a deadline
arm, where the losing arm should not run on.

Under `RunToCompletion` on the SQL tiers, ownership of unfinished children
transfers to the queued-work driver as a group-drain item keyed by the group,
claiming each unfinished child through its own existing lease and reusing
lease-expiry takeover rather than inventing loser-specific recovery. **The drain
reads the disposition declared on the group row and applies it; it never invents
a policy at drain time.** A group whose row declares `Cancel` therefore has its
losers cancelled by the drain even though the crashed caller never reached its
close — which is the whole point of moving the declaration to open. On Restate
and Temporal the transfer is a no-op — the engine owns it. The drain is a second
concurrent allocator against the group counter, which is safe only because of
the single-row atomic bump above; with a read-then-max allocator it would have
been an active corruption source rather than a passive one.

### Tier split

`supports_effect_groups()` is `true` on every in-tree tier **as target state** —
no controller answers `true` in the contract layer that introduces these types,
and each tier flips its own flag as it lands. Groups add **no second durability
flag**. The durability claim stays the existing
`replay_ownership` / journal-addressing fact, which the contract already warns
is only a routing fact and not an end-to-end durability claim.

- **In-memory/inline** implements the full observable semantics — wake,
  ordering, loser completion, disposition — in memory, durable only within the
  runtime's life. It is the behavioral reference and the conformance definition
  of the contract's *semantics*. It stores no journal entries at all, so
  persistence work never lands there.
- **SQLite and Postgres** implement the durable form and are the only tiers
  where the crash-permutation hazard is actually closed.
- **Restate and Temporal** get it from the engine. A Restate child is a full
  invocation rather than an inline journaled step: `ctx.run` cannot be held
  un-awaited, and Restate is ordinal-addressed, so a recorded body emitting a
  nested command would shift every later ordinal. A group must therefore
  dispatch children from outside any recorded body, which child-as-invocation
  satisfies by construction.

That split is the one ADR 0012 already made. Groups inherit it rather than
introducing a new axis.

The dialect surface is deliberately **not** gated on the host. Lowering is
compile-time and controller-blind, so `race`/`any` become accepted on every tier
including inline, where nothing is journaled. This is accepted explicitly rather
than worked around: the accepted surface is a property of the dialect, pinned
per session (ADR 0061) and enforced by one census and one register (ADR 0064),
so making it vary by tier would fork the census into per-controller variants and
the surface would stop being checkable. It is also how `sleep` and `waitSignal`
already behave — accepted everywhere, durable only where the tier is — and
ADR 0012's inline consequence is the standing disclosure.

### What this ADR does not fix

The DDL lives in code, not here: each SQL substrate owns its own tables and
columns, exactly as it already owns the replay table. A group table in a SQL
substrate is that substrate's *implementation of* this contract, not lash
substituting its own partial-order journal for the contract — the split ADR 0012
settled. Restate and Temporal implement the same contract with no such table.

## Alternatives rejected

**Per-leaf partial outcomes in one sealed batch record.** Keep one journal
entry, rewriting its payload as each leaf settles and sealing at the last. Three
failures. The ordering would live in a payload lash writes and interprets rather
than a fact the engine owns, which is precisely the machinery doctrine forbids.
It cannot express what this exists for: the record is guarded by one lease held
by the parked caller, so "the winner resumes while losers keep running" has no
owner for the losers and no way to release the caller without sealing. And a
record growing with every partial settlement is a read-modify-write hot row
unbounded in leaf count, straight at ADR 0025's bounded-journal obligation.

**A lash-owned partial-order journal, or a second ordering table.** ADR 0012 is
explicit that the contract grows by the smallest primitive an engine must get
right. A second ordering journal is the process-event-log alternative 0012
already rejected in a new costume, and it strands Restate and Temporal, which
own this natively.

**Always cancel the losers.** Much simpler — no background ownership, no
redrive question, no unbounded loser tail — and not ECMA-262. It survives as
`LoserDisposition::Cancel` for the deadline arm, where it is correct.

**Synthesize race from the atomic batch** by reporting the first entry of the
settled order. The batch still waits for every leaf, so the timeout idiom never
fires early: a `race` wrong in exactly the case it is written for.

**Fork on wake policy and migrate `Promise.all` later.** Rejected by ruling in
favour of one path. A fork would have kept two producers of `settlement_order`
alive across a release, and the later migration would then have had to prove the
*second* producer correct against a validator already loosened to accept the
first.

## Consequences

**A group turns one journaled effect into n.** Restate's segment boundary counts
executed effects against its budget, so a 50-leaf group costs 50 instead of 1
and trips boundaries sooner. This reaches every batch, not only race/any. The
design does not fight it: counting children individually is *honest*, and
ADR 0025 makes budget accounting the controller's obligation.

**All n children of a group contend on one allocation point** for the duration
of their finalize transaction. For race/any this is immaterial — groups are
typically two arms and the winner's latency is what matters. For wide aggregates
it converts independent per-child writes into a queue. The remedy is
pre-identified and **backend-local**: a per-group sequence generator, which takes
no row lock and does not participate in transaction rollback, or a hash-sharded
counter summed at read. Both keep strict monotonicity and merely widen the gaps
rank already tolerates, and neither moves this contract — which is why
committing to one path before measuring is safe. One precision so the escape is
not taken as free: it does not relax the finalize ordering above. The generator
is still called only *after* the fenced write reports rowcount 1, inside the same
transaction; what changes is that a rolled-back transaction burns a number
instead of leaving the counter untouched.

**Restate gets its first concurrently-settling batch, on every aggregate.**
Everything downstream of `settlement_order` on that tier was previously
exercised only against input order. What deletion of the serial branch removes
is not a falsehood but a *tier divergence*: Restate stops being the tier where
settlement order is trivially input order, and any consumer that quietly relied
on that will now see real permutations.

**`Cancel` is genuinely new work on Restate.** Resolving an awakeable with a
cancelled resolution unblocks a waiter; it does not cancel a running invocation,
and with child-as-invocation there is no invocation-cancellation path to reuse.
Under `RunToCompletion` the losers' durability must come from each loser's own
invocation completing and journaling its own outcome, **not** from resolving the
parent's awakeable: the parent may already have ended, and an awakeable of an
ended invocation is unresolvable, so putting a loser's terminal there would
silently lose it. The awakeable is the wake signal only.

**Two contracts move, and only one of them is the effect-host contract.** The
effect-host contract never sees the occurrence ordinal — it receives a finished
group key. But the VM↔host ability signature does move to thread that ordinal
from the VM, which knows how many times a call site has been reached, to the
host, which builds the key.

**Deviation 15 retires.** With every aggregate on first-settlement wake, `all`
reports at its first consumed rejection while losers run on under
`RunToCompletion` — which is what Node does — so the recorded deviation on
aggregate rejection timing has nothing left to describe.

**Later phases need no contract change.** Async-callback interleaving needs N
simultaneously suspended callback frames, which in contract terms is N
independent handles; all three methods are keyed by a handle and carry no ambient
per-caller state, so "how many callers are suspended at once" is a question this
contract never asks and cannot be made to ask. The work is VM-side continuation
encoding. A typed signal/deadline select is a two-child `First` group over two
existing commands with `Cancel` disposition — no new command, no new method.
