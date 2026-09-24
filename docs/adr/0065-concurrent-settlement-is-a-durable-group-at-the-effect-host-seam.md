# Concurrent settlement is a durable group at the effect-host seam

## Status

Accepted. Amended 2026-08-19 (FIG-1578): a group carries envelopes and nothing
else, and what runs a child is the host's registered `GroupExecutors` resolver
rather than a caller-supplied executor vec paired with the group.

Amended 2026-09-21 (FIG-3392): a tool child's loser lifetime is `live → closing
→ settled` rather than unconditional run-to-completion, one rank authority per
group is named explicitly, close releases consumer interest instead of deleting
group state, and FIG-1416 ruling 3 is superseded for unfinished tool execution
at normal opener end. The full contract is
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md),
which is implemented (see its Status); the clauses this ADR itself changes
are marked inline and summarised in
["Tool children have an opener lifetime"](#tool-children-have-an-opener-lifetime-fig-3392)
below.

Amended 2026-09-24 (FIG-2266, FIG-3394, FIG-3397): the contract drifted from
the text below as it was built. FIG-2266 deleted the `supports_effect_groups()`
flag, and the three group methods lost their default bodies: a controller
implements groups or refuses all three with `EffectGroupUnsupported`. FIG-3397
deleted `supports_concurrent_effects` with the tool-batch path, and made
Restate group children `call` children. FIG-3394 folded the occurrence ordinal
into `batch_id`, so the group key has no separate occurrence segment. The
affected clauses carry inline notes.

Amended 2026-09-24 (FIG-3669), **not yet implemented**:
[ADR 0104](0104-restate-is-the-only-effect-engine-sql-stores-are-storage.md)
makes Restate the only effect engine and the SQL stores storage only. This ADR
specifies SQL-engine behaviour: the SQL tiers' group rows, finalization and
store drain; the group contract stays, as an engine obligation. Those passages
stay as written until the PR that deletes the code (FIG-3667, FIG-3668, or
FIG-3600 for the session lease) rewrites them.

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

### Restate satisfaction: the group virtual object is the rank authority

That account is closed (ruled 2026-08-19). The Restate host satisfies the cursor
rule with a **group virtual object keyed by the group key**, using Restate's own
keyed state as the addressable store the durable-futures reading lacks.

A child, at completion, one-way-sends its settlement to that object, idempotent
on the child's replay key. The handler — single-writer by Restate's own
per-key guarantee — assigns `rank = counter + 1`, writes the settlement under
that rank, and resolves the parent's wake awakeable, all in one durable handler
step, so rank assignment and rank readability are one journal entry rather than
two. `await_next_settlement` reads rank `consumed + 1` out of that state first
and only otherwise waits, which is the recorded-first read the determinism
argument rests on. A fresh invocation holding none of the original futures
performs the same read, so the rule holds verbatim: the host keeps no per-caller
consumption state, any settled rank is re-readable until the group is closed,
and awaiting rank `consumed + 1` before and after a crash yields the same
settlement. The **shape fence lives beside the counter** in the same object, so a
reopen under a shrunk or reshaped group is refused by the same single writer that
allocates ranks. Ranks outlive the parent invocation, which is what lets
`RunToCompletion` losers rank themselves after the caller has gone.

Three consequences are ratified with it.

**The deployment surface grows.** An endpoint wiring this controller must bind
the group object's handler alongside the durable-wait services. A group is not a
thing lash can journal into a handler that is not there.

**Settlement payloads never transit object state as bytes.** The object
rank-indexes a *reference* into wherever the child's output already lives on
Restate — the existing large-payload discipline — so object state stays a
counter, a shape fence, and a rank → reference map.

**Close is idempotent, and it releases the caller's interest without deleting the
object's state.** *(Amended by FIG-3392: this clause originally read "Close
deletes the object's state, and the delete is idempotent". Deleting at close
destroys the rank, discharge and projection authority a committed-but-undrained
child still needs — see
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md) §4.
State is released at **retirement**, which requires discharged obligations, no
retained consumer dependency, and a surviving identity fence.)*
`close_effect_group(Cancel)` returns once one idempotent cancel per **eligible**
unsettled child has been durably issued — keyed by the child's replay key, so the
handle is derivable from state the group already carries and nothing extra is
recorded at open — and does **not** await confirmed loser termination. Awaiting it
would
block the winner on loser teardown, re-introduce the unbounded-tail coupling
`Cancel` exists to sever, and make Restate stronger than the contract, which is
itself the divergence this ADR exists to prevent. A loser's actual terminal —
cancelled, or completed in the race — still routes through the object for rank
like any other settlement, so a `wait_signal` deadline may return `Expired` while
the losing arm winds down for a bounded moment; that is the ECMA-262 race
semantics ADR 0062 already ratified and the same promise the SQL tiers make. A
redriven close re-issues the same idempotent cancels harmlessly.

**There is no TTL on that state, ever.** It lives until an entitled caller
reopens, consumes, or closes the group — the same continuation-not-audit reading
the SQL tiers take and the same standing no-TTLs rule claims take. Reclaiming a
truly severed group is ownership-severance work (FIG-1494), never a clock:
time-based deletion would make Restate the one tier where a saved continuation
becomes unresumable by waiting, which contradicts the refused-not-clamped
philosophy of the cursor rule itself. Because that leak is real until severance
lands, the host **exposes open group keys through its existing inspection
surface**, so orphaned groups are countable before there is a lever to reclaim
them.

Two alternatives were rejected. Narrowing the cursor rule per engine forks
contract semantics and makes a restored frame's saved cursor unresumable on one
tier. Mirroring settlements into the lash SQL store rebuilds the effect journal
lash does not own, and a Restate deployment may not mount a SQL store at all.

## Decision

**Concurrent settlement is a structured, durable group at the effect-host seam.**
The contract gains three methods on `RuntimeEffectController`, beside
`execute_effect` — not on
`AwaitEventResolver`, and not on `EffectHost`, whose levers are deployment-level
while group state is scope-level.

The contract says **durable child completion and first-settlement wake** and
names no engine primitive. A conforming host must be able to (a) start N
children as independently durable units, (b) report which settled first as a
durable fact, and (c) let the losers finish under its own ownership after the
caller has moved on.

- *(Amended by FIG-2266: the `supports_effect_groups()` flag this bullet
  describes is deleted. The three methods have no default bodies; a controller
  implements groups or refuses all three with `EffectGroupUnsupported` through
  `effect_groups_unsupported`, and journals nothing when it does. The bullet
  originally read:)* `supports_effect_groups()` — checked once at **deployment
  validation**, not per call. The group path is the only tool-batch path, so a controller
  answering `false` has no batch path at all, and a host wiring one should learn
  that at startup rather than mid-turn on its first `Promise.all`. It gates
  admission, not dispatch. A host may answer `true` **only if it has a registered
  `GroupExecutors` resolver**, because that resolver is where the `'static`
  executors come from: a child must be able to outlive its caller to honor
  `RunToCompletion`, and the borrow-scoped executor `execute_effect` takes carries
  the one lifetime this contract exists to break. The two capabilities are one
  question and must not drift apart, so on the in-process tiers this answer *is*
  "a resolver is registered" rather than a constant.

  It is therefore a **per-deployment fact established at wiring time**: before
  the resolver is registered a host answers `false`, and deployment validation
  reads it *after* wiring, which is where the check belongs in any case — a
  deployment is validated once it is assembled. The coherence law follows the
  flag across the whole surface: a host answering `false` refuses
  `open_effect_group`, `await_next_settlement` and `close_effect_group` alike
  with `EffectGroupUnsupported`, and journals nothing when it does. An unwired
  host is not a host with a bad group; it is a host that does not do groups, and
  one code for the whole surface is what lets a caller tell that from a group it
  cannot route.
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
  child failed — so no terminal is ever synthesized from a miss. On a **first
  open** a host resolves **all N children before it records the group**: any
  `None` refuses the whole open with a typed group-shape error naming the child's
  position and replay key, and **nothing of the group is journaled — the group
  row included**, so a group is never half-opened around a child that will never
  settle.

  That the group row is inside the refusal is load-bearing rather than tidiness
  (FIG-1579). Whether a miss refuses is decided by whether the journal already
  holds the group, so a host that wrote the row and then refused would answer the
  operator's retry as a **reopen** — and a reopen passes a miss through by design.
  The second attempt would open successfully, dispatch only the children it can
  route, and park its caller forever on a rank nothing will allocate: a strand one
  attempt after a refusal, which is worse than the refusal it replaced. Two
  identical refusals are the observable form of "the refusal journals nothing".

  A **reopen** — a group the journal already holds — resolves only what it
  dispatches and refuses nothing: a journaled group meeting a host that cannot
  run one of its children is a deployment change rather than an open, and the
  drain reports it as `NoExecutor`, leaving the group unreclaimable and visible
  rather than inventing a terminal for it. Refusing at a reopen instead would
  deny a resuming caller the ranks the group already holds, which is the opposite
  of what the refusal is for. A host with **no registered resolver at all** still
  hands out a drain, and its passes report every child as `NoExecutor` for the
  same reason: the queue is real, this host cannot finish it, and reporting an
  empty pass would mark the group reclaimable.

  **The engine tier's form of the same fact is a 404.** A child invocation
  addressed to a service or handler no registered deployment binds is a routing
  fact too, and it must surface as the engine's own terminal — on Restate, a
  `TerminalError` carrying `404` — never as an ordinary handler error. Restate
  retries a handler error with infinite exponential backoff, so a retryable 404
  turns a forgotten `bind` into an invocation backing off forever with nobody
  told what is wrong: the engine-tier twin of the warn-and-strand above.

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

The three group methods have no default bodies (FIG-2266): an out-of-tree
controller that has not implemented groups returns
`RuntimeErrorCode::EffectGroupUnsupported` explicitly and fails closed with a
named error rather than mis-executing a batch.

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
unbounded durable await. *(FIG-3392 completes the second half: `race([])` keeps
ECMA-262's never-settling semantics, no group is opened, and the host reports a
typed unsettled-await failure rather than parking. That is a host lifetime
contract in [ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md),
not a deviation-register entry; see
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md) §11.)*

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

A group's key is `{scope_id}:group:{batch_id}`, or
`{scope_id}:group:{parent_effect_id}:{batch_id}` for a nested batch. *(Amended by
FIG-3394: the key originally carried a separate `:{occurrence}` segment; the
occurrence ordinal is now folded into the `batch_id` hash, so it is carried
exactly once. The reasoning below still holds for that ordinal.)*

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

`LoserPolicy::RunToCompletion` is the default for `race`/`any` because it
is what ECMA-262 specifies: a losing promise keeps running and its side effects
still happen. Cancel-always would be simpler — no background ownership, no
redrive question, no unbounded loser tail — but it would be a silent divergence
in exactly the family ADR 0062 forbids.

`LoserPolicy::Cancel` survives as the *correct* semantics for a deadline
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

Every in-tree tier **implements** the group surface as target state — no tier
does so in the contract layer that introduces these types, and each one lands its
own implementation. Whether groups are available is not a property of the
tier, though: an in-tree tier whose host has not been handed a `GroupExecutors`
resolver refuses the whole surface coherently with `EffectGroupUnsupported`
(FIG-2266 deleted the `supports_effect_groups()` flag that used to answer this). Groups add **no second
durability flag**. The durability claim stays the existing
`replay_ownership` / journal-addressing fact, which the contract already warns
is only a routing fact and not an end-to-end durability claim.
*(Superseded: this fact is now the one sync `RuntimeEffectController::effect_journaling()` → `EffectJournaling { Local, Journaled }` (FIG-2226).)*

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
than worked around: the accepted surface is a property of the dialect (ADR 0096)
and enforced by one census and one register (ADR 0064),
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
`LoserPolicy::Cancel` for the deadline arm, where it is correct.

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

## Tool children have an opener lifetime (FIG-3392)

**Implemented** by FIG-2266, FIG-3396 and FIG-3397, and
[ADR 0099](0099-tool-children-of-effect-groups-are-live-closing-settled.md) is
the contract. This section records what changes in *this* ADR.

### The loser lifetime is three phases, not one disposition

`LoserPolicy::RunToCompletion` above says what happens to a losing child *while
its opener lives*, and this ADR wrote it as though that were the whole lifetime.
A tool child now has three phases — **live**, **closing**, **settled** — defined
in ADR 0099 §0. Selection still cancels nothing while the opener lives, which is
the ECMA-262 fidelity this ADR already required.

**An opener is `Turn(session_id, turn_id)` or `Process(ProcessRef { process_id,
incarnation })`**, stable across worker attempts and segments and changing on
process re-registration. Today's `ExecutionScope::Process` carries only
`process_id`, so it does not establish that identity; FIG-3394 binds the
incarnation into the shared group and child identity, and FIG-3396 validates it
during recovery. A retired or mismatched incarnation is refused, never rebound to
the current process of the same name.

**A dead worker is not a closed opener.** ADR 0094 calls an uncommitted crashed
turn "interrupted, not ended", and the drain guards in `group_drain.rs` are
expressly local — "A group open in *another* process is not distinguishable from
a closed one here". Recovery therefore classifies an opener by a **durable
live→closing transition recorded before admission stops or any cancellation is
issued** (ADR 0099 §7), not by a lease or a terminal that a crash may precede.

### Commit and cancel meet at one linearization point

**Cancellation requests are not cancellation decisions.** The final-attempt
record and the cancel disposition compete at one durable, fenced linearization
point, and exactly one commits. A winning final record retains settlement
ownership; a winning cancel disposition refuses any later attempt-final record.
The bounded grace follows the decision and cannot reverse it.

**Rankability follows the child's own obligations, not opener close.** A
committed final proceeds through intent drain and durable projection to rankable
while the opener is live or closing, so a live aggregate reaches rankability.

**Within a group, intent drains are admitted in final-commit order**, the durable
order of that linearization point, rather than in the batch's source order. This
ADR's whole purpose is to make settlement order a durable fact, and that is what
makes the replacement available: cross-child source order was a determinism device
chosen when completion order was re-raced on every redrive. Carrying it into
groups would have made `Promise.race([slow(), fast()])` unable to resolve with
`fast` — every terminal leaf takes its source turn today, whether or not it
declared an intent — and one hung source-first tool would stop every sibling from
ever ranking. Rank order therefore equals commit order; only drain duration
varies, and cancel-decided children still route through for rank like any other
terminal.

**A committed-but-undrained final remains unsettled and is ineligible for
cancellation.** It is excluded from *cancellation*, not from lifecycle
accounting: it still needs the rank, discharge and projection authority this ADR
provides. Issuing a cancel for it would drop a recorded declaration ADR 0042
protects, and would make the presence of a deadline arm decide whether a recorded
trigger or start exists.

**Cancel/abandon is a durable, fenced disposition, distinct from physical stop.**
It forbids new unprotected semantic admission under the cancelled invocation; it
does **not** undo already-admitted commands or cancel committed descendant
obligations, which retain authority to finish under the original opener.
Orchestrating children — which ADR 0042 says have no `ToolAttempt` frame of their
own — are classified by their retained command and child obligations, never by an
invented outer attempt. Known usage is never fenced out (ADR 0099 §13).

**Restate engine cancellation is never the sole close protocol.** The pinned
shared core cancels tracked child calls from inside `do_await` without consulting
any Lash commit fence, so switching `.send()` to `.call()` does not by itself
protect a committed final. The journaled cooperative path and retained
protected-work recovery must preserve these rules even when implicit cancellation
interrupts a child invocation.

### One rank authority per group

Restate's notification order is **invocation-local**: the protocol requires a
replaying SDK to observe the same relative order it observed while processing,
which serves a consumer replaying one selection schedule inside one invocation.
Attaching already-finished children from a *successor* segment yields new
notifications, not the predecessor's observation order.

**SDK notification order may serve as rank only where same-invocation replay
proves the whole contract.** The group virtual object above stays wherever
consumption, discharge or retained observation crosses invocations, and removing
it requires proof of the cross-segment contract. The consumed prefix and the
settlement payloads a successor still needs persist across handover; expired
Restate attachment is a typed recovery failure, never permission to rerun a side
effect.

Two corrections to this ADR's earlier reading of the engine tier:

- **Implicit child-call cancellation covered zero group children.** The
  pinned VM cancels tracked request-response children and deliberately exempts
  one-way sends, and every group child was dispatched one-way (`.send()`). The
  per-child CANCEL durable wait was the only cancel path those children had, and
  could be removed only in the same cutover that made children `call` children.
  *(Amended by FIG-3397: group children are now `call` children
  (`crates/lash-restate/src/effect_group/dispatch.rs`), so implicit cancellation
  tracks them.)*
- **The "a Restate group child is an invocation" ruling survives, but not for the
  reason given.** "The Rust SDK requires `ctx.run` closures to be awaited
  immediately" is a Rust-SDK binding gap with an upstream issue, not a protocol
  constraint: the shared core lash links models a *set* of executing runs, and the
  TypeScript and Java SDKs compose runs today. The conclusion stands on better
  ground — a child invocation gets its own retry policy, suspension, native
  cancellation and idempotency-key identity, and keeps the leaf attempt body out
  of the parent's ordinal journal.

### FIG-1416 ruling 3 is superseded, narrowly

FIG-1416 ruling 3 assigned losers to the queued-work driver under both
dispositions and rejected leaving them on the opening scope's task set. **That
ruling is superseded for unfinished tool execution at normal opener end, and for
nothing else.** Recovery ownership of protected settlement stays, as do the
drain, the disposition-at-open rule and the work-driver seam. The owner gives up
implicit durable background tools — an unfinished loser is no longer guaranteed
to eventually perform its opaque work or produce new intents after `finish` — and
keeps the explicit process, whose ADR 0094 lifecycle is unchanged.

The Node comparison lives in
[ADR 0062](0062-the-typescript-dialect-is-an-exact-ecma-262-subset.md)'s matching
amendment: within a live opener nothing diverges from Promise non-cancellation,
and the divergence at opener end is a host lifetime contract measured against a
Node host that stays alive after an async function returns.

### Admission bounds retained work, and retirement is not close

"No segment boundary while tool children are unsettled" is refused: it fails
[ADR 0025](0025-bounded-journals-are-an-effect-controller-obligation.md) for a
days-long process racing one quick tool against one hung tool at width two.
Outstanding children are reattached across segments instead.

A group's open admits, as a typed refusal of the **whole open before dispatch**,
a bound on retained work **per exact logical opener** — nested, accepted-unclaimed,
running, closing **and settled-but-still-required** children and group metadata —
counting unique executions separately from operand positions, with capacity
reserved atomically at acceptance, reused by replay, and released only when a
child's recovery and consumer dependencies are discharged. Accepted work is never
retroactively refused by a changed budget. Backend command headroom for
parent-side dispatch, observation, cancellation, incorporation and handover is a
**per executing controller/segment** bound, not one counter over a days-long
opener; FIG-3397 names those accounting units and their release conditions.

**A completed group may retire as a whole while its opener remains live**, once no
replay or continuation needs it and an existing identity fence prevents
resurrection. Retiring the live opener's entire scope to retire one group is not
available, and retirement stays group-atomic (N3 above). Mid-aggregate VM
suspension is required only if command accounting shows a bounded aggregate cannot
meet the controller budget, and no universal tool-result byte cap follows.
