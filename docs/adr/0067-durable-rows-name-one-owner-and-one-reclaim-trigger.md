# Every durable row names one owner and one reclaim trigger

## Status

Accepted. Ratified on FIG-1494; the six sections of the decision are the six
rulings recorded there. The trigger-store ownership map was ratified on
FIG-1507 on 2026-08-21.

## Context

Lash's durable state grew one row class at a time, and each class arrived with
its own answer to "who deletes this, and when". Some rows die with a cascade
from the session that owns them. Some are vacuumed when the owner reaches a
terminal state. Some are swept by a host-invoked lever. Several are never
deleted at all, because nobody asked the question when the table was added.

That drift is not a tidiness problem. It produced defects with a common shape:

* **Rows with no owner at all.** Dedup and time-window tables
  (`tool_intent_submissions` is the canonical one) were reasoned about as
  caches with a natural expiry rather than as durable rows belonging to
  something, so they had a retention *policy* and no *owner*. A retention
  policy answers "how long", never "whose".

* **Timers reaching live rows.** Where reclamation was armed by age rather
  than by the owner's terminal transition, nothing structural stopped a sweep
  from condemning a row whose owner was still running. Grace windows made this
  rare, which is the worst frequency for a correctness bug.

* **Sweeps that could not tell empty from blind.** FIG-1246 was an attachment
  sweep that read an unenumerable root set as an empty one and deleted live
  bytes. The enumeration failed; the caller saw `{}`. Nothing in the type
  distinguished "I looked at everything and there was nothing" from "I got
  nothing back". FIG-1508's process registry is the same shape, still unwired.

* **Destructive work inside the wrong transaction.** SQLite runs a whole-DB
  blob sweep inside the session-delete transaction while Postgres does no
  session-scoped reclaim there at all (FIG-1506). Neither is right: one makes a
  bounded owner-cascade unboundedly expensive and fail for reasons unrelated to
  the delete, the other leaks.

* **Failures laundered into clean reports.** A backend that caught its own
  error and returned an empty report was indistinguishable from a backend with
  nothing to do, so a sweep that reclaimed nothing for weeks looked healthy.

The unifying observation is that these are not five bugs in five subsystems.
They are five consequences of never having stated the reclaim model. This ADR
states it. Every FIG-1494 child specs against the sections below.

The model binds **row classes and owners**, not today's trait names. Shape C
(FIG-1280) will move where these rows live; it does not change who owns them or
what arms their reclamation, and this ADR is written to survive that cutover.

## Decision

### 1. Universal ownership axiom, with no exceptions

**Every durable row class names exactly one owner and exactly one reclaim
trigger class.**

The owner is one of: session, turn, process, effect group, factory. The trigger
class is one of: owner-delete cascade, terminal-state vacuum, or drain-armed
sweep.

No row class may exist without a named owner and a named trigger. "It is small",
"it expires anyway", and "the host can truncate it" are not answers. A new table
whose owner and trigger are not stated is not reviewable, and a table that
cannot name an owner is evidence that the thing it records belongs to something
that does not exist yet.

#### Reclaim is severance, not sweeping

The trigger classes above sit under an earlier and stronger ruling (FIG-1494,
2026-08-17), which this section does not relax:

> **Reclaim is a transactional consequence of the operation that severs
> ownership — never a sweep.**

Three rules follow, and they decide which trigger class a row class may name:

1. **Classify every row class as singly-owned or multi-referenced.** Singly-owned
   classes (turn commits, usage deltas, pending inputs, manifests, lineage,
   trigger state) get *no reference machinery at all*: the owner's severing
   transaction reclaims them inline, and every root-set or GC touchpoint they
   carry today is removed wholehog. Multi-referenced classes (blobs across heads,
   anchors and artifact refs; fork-shared ancestry nodes; attachment digests) are
   the only ones a reachability question applies to.

2. **Reference edges are data, not stored counts.** A multi-referenced row's
   deletion is gated by an indexed `NOT EXISTS` over exact edges, evaluated
   inside the severing transaction — the Nix `Refs(referrer, reference)` shape.
   Counts drift; edges cannot, because they are the data. The reference graph is
   a DAG by construction, so edge-gated reclaim is complete and no tracing
   collector is ever needed for correctness.

3. **Mark-and-sweep is demoted to the read-only verify/repair tier.**
   `gc_unreachable` becomes a host-invoked auditor in the
   `PRAGMA integrity_check` / `nix-store --verify` tier. **Correctness never
   depends on it running.** A verifier that finds corruption stops and reports;
   it never "repairs" by freeing.

So the three trigger classes of this section are not three flavours of sweeping.
Owner-delete cascade and terminal-state vacuum *are* the severing transaction —
they are how "never a sweep" is implemented. Drain-armed sweep is the bounded
exception, and it reclaims only what no single severing transaction can reach:
factory-scoped residue and multi-referenced rows whose last edge was cut by a
transaction that has already committed or died. A singly-owned row class that
names a sweep as its trigger is misclassified, and the fix is to find its
severing transaction, not to schedule the sweep more often.

#### Drain-armed sweep, defined

A **drain-armed sweep** is reclamation armed by a live drain running under the
session-execution lease it is reclaiming for. It is not a scheduled scan and not
a background actor: it exists only where a drain is already executing, already
fenced, and already authorized over exactly those rows.

The constraint is deliberate. A sweep that reached sessions with no live drain
would need a cross-session repair actor holding no session-execution lease —
precisely the fencing shape FIG-1573's fix exists to forbid, and "never delegate
fencing" applies. The sweep stays drain-armed; the gap it leaves is closed by
naming an owner, not by widening the sweep.

That gap is a row class in its own right (ruled 2026-08-19 from the FIG-1573
field confirmation): **turn-input rows owned by sessions that will never drain
again.** A fingerprint rotation left an old session's `pending_active`
turn-input row stranded permanently, because the dormant session never drains
and the repair path is drain-armed. The row is not a continuation defect of any
live session; it is storage residue, and it is reclaimed as such — *session
dormant or rotated, and the row in a pre-claim state* is a reclaimable state,
under the same witnessed-emptiness rule (section 5) as everything else.

### 2. Terminal-before-reclaimable (Amendment 1)

**Reclamation is armed only by the owner's terminal transition.**

A per-class grace delay may *defer* reclamation. It may never *initiate* it. No
timer may ever reach a non-terminal row: age is a retention knob applied after
terminality is proven, never a liveness oracle.

Enforcement is structural, not procedural. On the SQL backends the DDL permits a
row's reclaim-eligibility timestamp to be non-null only in terminal states, via
a CHECK constraint in River's `finalized_or_finalized_at_null` shape, so the
invariant is unfalsifiable at the schema level rather than being a property of
sweep code (FIG-1606 scopes this to SQLite and Postgres). A backend with no DDL
to carry the constraint — the in-memory store, a future non-SQL substrate —
demonstrates the same invariant through the conformance law instead: reclaim
eligibility set on a non-terminal row is red. What is not available is the third
option of demonstrating it nowhere.

Terminality arms *eligibility*; it is not by itself authority to execute. ADR
0023 still governs execution for every class a host projector observes: the host
supplies the `RetentionBound` and the projection watermark, and reclaim never
runs past an unacknowledged cursor. The two rules compose in one direction only
— terminal *and* within the host's bound — and this ADR does not supersede or
amend ADR 0023. Age remains a bound applied only after the owning scope is
terminal and after the relevant watermark, exactly as 0023 states it.

There are no dedup or time-window carve-outs:

* `tool_intent_submissions` takes its owner from emission scoping — the
  submission belongs to the emission that produced it (FIG-1599, FIG-1509).
* A trigger occurrence's owner is its delivery fan-out, in both directions. An
  occurrence that matches zero deliveries is terminal at ingest-accounting time
  and therefore immediately reclaimable. A *matched* occurrence becomes
  reclaimable when its last delivery reaches a terminal state — which is what
  makes the `trigger_deliveries` cascade stop being inert, since today the
  parent never dies. FIG-1507 specs against both arms; neither is an exception
  to the axiom, they are the axiom applied to a fan-out owner.

### 3. Scope-split trigger topology

**Owner-scoped reclamation runs inline with the owning transaction. Factory-global
reclamation runs only through host-invoked levers.**

Owner-delete cascade and terminal-state vacuum run in the owner's transaction,
bounded strictly to the owner's rows: deleting a session reclaims that session's
rows and nothing else, deterministically. A reclaim error inside that scope
**fails the delete honestly** rather than leaking silently — if the cascade
cannot complete, the delete does not claim to have happened.

Factory-global work — anything whose cost scales with the store rather than
with the owner — runs only when the host invokes a lever. There is no
commit-triggered auto-sweep configuration in the contract. A commit path that
occasionally becomes a full sweep is an unbounded latency cliff the host never
asked for and cannot schedule around.

The FIG-1506 asymmetry resolves in both directions at once: SQLite's whole-DB
blob sweep moves **out** of the session-delete transaction, and Postgres gains
session-scoped reclaim **in** it.

#### Trigger-store ownership map

Trigger retention is scope-shaped. A session owner can become permanently
unable to speak through ADR 0049. A host or platform owner has no equivalent
terminal frontier, so its name fence remains durable.

| Row class and scope | Owner | Reclaim trigger |
|---|---|---|
| Session subscription | Registering session | The ADR 0049 deleted-session frontier. Delivery-retention reconciliation deletes the row in its trigger-store transaction only after witnessing zero remaining deliveries for the subscription. This applies to enabled and tombstoned rows; a tombstone remains the `Revive` CAS fence while its session could still speak. |
| Host or platform subscription tombstone | Host or platform namespace | Never. It is the permanent `Revive` name fence, and there is no purge lever. |
| Session mutation receipt | Registering session's replay eligibility | The same ADR 0049 frontier and trigger-store reconciliation transaction. Receipts survive while any delivery owned by that session remains. Once the frontier is crossed and the delivery set is witnessed empty, post-deletion replay is impossible and the journal is reclaimed. |
| Host or platform mutation receipt | Host or platform replay eligibility | The existing host-invoked `prune_mutation_receipts` cutoff. This policy is unchanged. |
| Trigger occurrence | Committed delivery fan-out | Delivery-retention reconciliation deletes the occurrence only after witnessing zero remaining delivery rows. A zero-match occurrence has a committed empty fan-out at ingest, so the same predicate reclaims it. A matched occurrence waits for its last delivery. |
| Trigger delivery | Deterministic process run | ADR 0021 process retention. This policy is unchanged. |

The owner namespace added to new receipt JSON is not retroactive. Legacy
successful mutation receipts and non-empty prune receipts already carry a
typed owner in their record snapshots, so retention extracts that genuine
owner. Legacy conflict/error receipts and successful empty-prune receipts do
not carry an owner anywhere; their scoped receipt key is a one-way hash and
cannot recover it. Those ownerless rows are retained indefinitely by design
rather than classified lossily. They are a bounded pre-change set: the
classifiable legacy portion shrinks as sessions cross the deletion frontier or
host cutoffs run, while the irreducibly ownerless remainder stays retained.

The reconciliation deletes exact terminal delivery candidates first. It then
applies the occurrence and dead-session cascades in one trigger-store
transaction. Any failure rolls the transaction back; retry repeats the same
decision without weakening subscription revision fencing or delivery claims.

### 4. Report contract: a three-way split

**Blocked is not error, and failure is not silence.**

A reclamation report distinguishes three outcomes, and the types make all three
reachable:

1. **Empty** — the scope was enumerated and there was nothing to reclaim.
2. **Blocked** — typed blocked reasons, each with a count, ride in `Ok(report)`.
   A row skipped because its owner is live, because a peer sweeper holds the
   condemnation, or because a grace window has not elapsed is a normal outcome
   with a name, not an absence.
3. **Failed** — always `Err`, and the error **carries the partial report
   accumulated before the failure**. Work already done is reported; the failure
   is not laundered by discarding it, and the caller is not told to guess how
   far the sweep got.

A conformance law reds any backend that swallows an injected failure into a
clean report. (Seam ticket: FIG-1505.)

### 5. Witnessed emptiness

**Every destructive scope boundary consumes a typed enumeration witness.**

"Enumerated completely, zero rows" is a different value from "enumeration
returned nothing". The second covers an error, a partial scan, and an unwired
registry, and it must not be spellable as the first.

An empty scope without a witness **refuses**, naming the source it could not
prove. A host-asserted empty root set is not an enumeration — FIG-881 stays
refused on exactly this ground. The type is what makes the FIG-1246 incident
structurally impossible rather than merely fixed: the failing enumeration has
no witness to hand over, so the delete cannot be authorized. FIG-1508's unwired
process registry likewise cannot witness, and is therefore refused from the
sweep, loudly, instead of contributing an empty set. (Type ticket: FIG-1607.)

A refusal here is reported, not thrown away: it surfaces through section 4's
blocked channel as a typed reason naming the unproven source, so "process-owned
rows were refused from this sweep" is a visible outcome rather than a sweep that
looks clean while silently rooting those rows forever.

**Amendment (2026-08-20):** A refusal is
`Err(MaintenanceFailure { stop: Refused(..), partial })`, not an `Ok`-side
counted reason. A refusal must be impossible to read as a healthy sweep, while
the partial report preserves the earlier ruling that work already reported is
not thrown away; this replaces section 4's `Ok(report)` blocked-channel
placement for refusals without changing the typed refusal reason.

### 6. In-flight destruction: adoption-first, generation-fenced

**A factory sweep begins by adopting the incomplete condemnations left by
crashed predecessors.**

**Condemnation rows are owned by the condemnation protocol itself**, which is
factory-scoped: the owner in section 1's list is the factory, and the trigger
class is the drain-armed sweep, discharged by the adoption pass below. Naming
that owner is what removes the row class's exemption from section 1 — a
condemnation is not protocol scaffolding that lives outside the axiom, it is a
durable row with an owner like any other.

The obligation is exact: **complete or release each adopted condemnation before
starting new work.** Adoption comes first, and it verifies blob state through
the same witness the sweep uses everywhere else — "the backend says the blob is
gone" and "the backend errored" are different answers, and only the first
completes the delete. FIG-1510's stuck-forever state becomes unreachable, with
no timer anywhere.

The generation pin is the **sweep pass's own generation**, not a session-lease
token: a factory sweeper holds no session-execution lease, so it cannot pin the
ADR 0029 fencing token. It borrows 0029's *shape* — a condemnation records the
generation of the pass that created it, and a later pass adopts only what an
older generation left — so two concurrent levers cannot adopt the same row. The
generation proves the old pass is dead; the witness decides what state the blob
is in. It is the same shape as an effect-group drain adopting lost runners.

`list_condemnations()` is the enumeration surface (FIG-1510), and its stated
purpose is operator inspection: "what is stuck right now" must be answerable
without waiting for a sweep to run.

**This refines ADR 0028's condemnation-recovery rule.** 0028 keeps its state
machine timestamp-free — that is unchanged and load-bearing — but it makes
clearing a condemnation left by a sweeper that died mid-delete *host policy*,
with the host calling `release_attachment_condemnation` after deciding the
sweeper is gone. Here that recovery becomes automatic and structural: the next
sweep adopts it, under a generation that proves the predecessor dead. The host
lever remains, and lash still expires nothing on a clock.

Effect-group VO state severs the same way. The effect-group row owns the group
VO's state. Group retirement issues the idempotent VO purge inline — the owner
cascade of section 3 — and the factory sweep gains a severance pass for group
keys it can prove are gone (FIG-1608, after the FIG-1537 wiring lands).

There is no TTL on VO state, ever. A TTL is a timer reaching a row whose owner
it never consulted, which section 2 forbids.

## Alternatives considered

* **Per-class retention policies instead of ownership.** Rejected: a retention
  policy answers "how long" and never "whose", which is exactly the gap that
  produced ownerless dedup tables. Retention survives as a deferral knob under
  section 2, subordinate to a named owner.

* **Keep an auto-sweep commit hook, bounded by a work budget.** Rejected: a
  budget bounds the sweep's cost per commit but not its blast radius, and it
  leaves the commit path holding destructive work the host did not schedule.
  Levers are the honest shape; a host that wants sweeping on a cadence runs the
  lever on a cadence.

* **`Option<Report>` or an empty report for enumeration failure.** Rejected:
  both spell "nothing" for two different facts. This is the FIG-1246 defect
  reintroduced at the report layer rather than the root-set layer.

* **A sentinel row or host assertion standing in for a witness.** Rejected: an
  assertion is a claim, an enumeration is evidence, and the destructive
  boundary must consume evidence. This is why FIG-881 remains refused.

* **TTL-expiring VO state to avoid a severance pass.** Rejected under section 2:
  it arms reclamation by a clock rather than by group retirement, so it can
  reach a live group.

* **A cross-session repair actor for rotation-stranded rows.** Rejected: it
  would hold no session-execution lease over the rows it reclaims, which is the
  fencing shape FIG-1573's fix exists to forbid. The stranded rows get an owner
  instead, and the sweep stays drain-armed.

* **Keeping `gc_unreachable` as a reclaim mechanism to harden.** Rejected by the
  round-2 ruling: it is demoted to the read-only verify/repair tier, so
  FIG-1504's "no production caller" finding argues for the demotion rather than
  for wiring a caller.

## Consequences

* Every new durable table states its owner and trigger class at review time.
  A table that cannot is a design finding, not a style nit.
* Commit-triggered automatic GC leaves the contract. Hosts that relied on
  incidental sweeping now schedule the lever, and the sweep's cost becomes
  something they can observe and place.
* Reclamation reports gain typed blocked reasons and an error that carries its
  partial report; backends that previously returned a clean empty report on
  failure fail the new conformance law.
* Destructive boundaries take a witness parameter, so an unenumerable source is
  a refusal at the call site rather than an empty set flowing inward.
* SQL-backed reclaimable classes carry a DDL CHECK tying reclaim eligibility to
  terminal state, so the "no timer reaches a live row" rule is enforced by the
  database rather than by review; non-DDL backends carry the same invariant as
  a conformance law.
* Singly-owned row classes lose their root-set and GC touchpoints outright
  rather than gaining better ones, and multi-referenced classes converge on one
  edges-backed predicate instead of the hand-copied liveness SQL.
* This ADR refines ADR 0028's condemnation recovery (adoption becomes automatic
  and generation-fenced; the host lever and the timestamp-free state machine
  both survive) and leaves ADR 0023 intact — terminality arms eligibility, the
  host's `RetentionBound` and watermark still bound execution.

### Children and sequencing

The model is implemented by the FIG-1494 children, plus three tickets filed for
the parts this ADR made explicit:

* **FIG-1505 — report contract seam. First.** Sections 3, 5 and 6 all report
  through it, so every other child either builds on this shape or is rewritten
  by it.
* **FIG-1607 — the enumeration witness type** (section 5), and **FIG-1606 — the
  terminal-state CHECK mechanism** (section 2). These are the two structural
  pieces; children that only consume them can land afterwards in any order.
* **FIG-1504–1516 — the per-class ownership and trigger work** (sections 1–3),
  including FIG-1506's SQLite/Postgres scope split and FIG-1510's
  `list_condemnations()` surface. **FIG-1509 is blocked on FIG-1599**: the
  emission scoping that gives `tool_intent_submissions` its owner has to exist
  before the row class can name one.
* **FIG-1508** stays open as a refusal, not a fix: the process registry is
  refused from the sweep until it is wired to witness.
* **FIG-1507** specs against both trigger-occurrence arms in section 2 —
  zero-match terminal at ingest accounting, matched terminal when its last
  delivery is — since implementing only the first leaves the cascade inert.
* **FIG-1608 — effect-group VO severance** (section 6), after FIG-1537.
* The rotation-stranded turn-input class (section 1) has no child yet; it enters
  the ownership map here and needs one filed against FIG-1494.
