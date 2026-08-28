# Tests must be independent of what they test

Recent escaped defects cluster at seams: durable-host interaction, the
host-facing API, example and host surfaces, providers, prompt assembly. Issue
79 is the shape — a tool attempt that production never emitted, in a subsystem
whose replay conformance suite was green throughout, because that suite
hand-constructs the envelope whose absence was the bug.

Two limits on that observation, both of which earlier drafts of this document
overstated past:

**Escape archaeology cannot measure prevention.** A unit test that fails before
commit leaves no artifact. "Recent escapes are seam-heavy" is supported by the
record; "the unit suite discovers little" is not measurable from a denominator
that excludes prevented defects by construction, and is not claimed here.

**Automated suites do discover defects, including internal ones.** The record
contains an application-suite catch in CI (`8ef74b93`), a cancellation race
caught on main (`af36f8fa`), three CI sightings of a durable-wait wedge
(`995b6881`), a missing request field caught by perf CI (`3473e91c`), and a
compaction recursion that hung tests and would have hung production
(`f7f94c31`). The last two are internal-logic defects found by ordinary
author-written tests, which failed for reasons their authors had not
anticipated. Any rule implying that cannot happen is false.

## The rule

> Do not derive a test's expected value by running the implementation under
> test, or by duplicating the transformation that could be wrong.

This is a rule about **independence from the code path**, not about authorship.
Author-written literals, state expectations, error classifications and API
contracts are all legitimate oracles. What is not legitimate is an expectation
the implementation computes for you, or a model that projects its own facts and
then verifies them.

Three sources of independent expectation, in increasing cost.

**Implementation-free invariants.** Properties stated without reference to how
the code achieves them: every provider-body invocation has a corresponding
attempt envelope; no runtime-originated span is a detached root; no sweep spawns
unboundedly. These generalise to call sites that do not exist yet, and they are
the only mechanism that catches an effect which was *never emitted* — replay
cannot, because an unemitted effect leaves nothing to diverge from.

**Differential execution.** Run the same scenario live, then replayed from its
own journal, and assert identical committed state and zero local re-execution.
This requires an **explicit redrive**. A replaying host that is never redriven
proves nothing, which the existing durable-input runbook demonstrated by passing
while the defect was live.

**Recorded reality.** Provider fixtures captured from real traffic, and
snapshotted prompts. The repository's context-overflow corpus was harvested from
real providers and has caught defects; a rate-limit fixture whose body was
invented could not discriminate correct throttle-deference from the hard-fail
bug that shipped. Synthetic fixtures remain appropriate for parser branches and
protocol metadata, where the wire shape is the specification rather than an
observation.

## Where a durability test stands

A test about durability enters above the emission point, through the public API,
against a host that actually replays. `NativeEffectHost` never replays; it is
the right choice for tests about something else and the wrong choice for tests
about durability. It is currently the overwhelming default, and changing that is
a migration, not a preference — see Consequences.

## What is not a test

Some escape classes are cheaper to prevent than to detect. An ambiguous outcome
that collapses distinct causes into one value is fixed by a type. Context
dropped across a spawn is fixed by a lint at every site at once. A dispatch path
that must not be reachable is fixed by visibility. These do not rot, they cover
sites that do not exist yet, and they cost less than the harness that would
detect their absence.

## Simulation

The generator and minimiser are valuable and stay. Model-layer oracles that
project their own facts and then verify them do not — but "cannot establish the
advertised property" is not the same as "cannot fail", and the difference must
be established per-oracle rather than assumed. Where an oracle consumes real
observed boundaries it is capable, whatever its name suggests; where it asserts
a property of the model, it should be renamed to say so rather than deleted.

The current clock advances virtual time and then yields a fixed number of times
hoping background tasks registered. That is not determinism, and it is the
mechanism behind at least one dismissed lease signal. Lash-sim is a scenario
generator over real execution, not a determinism harness. The
compile-and-enumerate spike is cancelled — not because the clock's one timing
signal proved ambiguous (that ambiguity was the broken clock's own artifact),
but on coverage grounds: the escapes this document opens with cluster at
real-substrate seams — SQLite threads, Postgres, Restate, live providers — and
an in-process deterministic lane cannot reach any of them, which the spike's
own scoping already conceded. Determinism would buy exhaustive interleavings
over the part of the system that escapes least. Rare in-process interleaving
bugs are therefore accepted as out of scope for lash-sim; if one escapes to
production, this is the decision to revisit first.

## Deletions

A test that cannot fail is worse than no test, because it reads as coverage. But
incapability is a claim requiring evidence, and the evidence is **mutation**:
break the production code the test claims to guard and observe whether it goes
red. Deletions without that evidence are not permitted.

FIG-528 is the worked example and the caution. Eight tests were nominated by
review; mutation testing kept three. The sim exactly-once oracle went red when
the replay controller was made to invoke the local executor, because it consumes
real observed boundaries — the audit had read the store in isolation and missed
it. Batch-ordering assertions went red when the production sort was reversed and
when serial tools were run in parallel. Review was wrong on three of eight;
mutation caught all three.

Where a deletion is justified, the commit states what property the test claimed
and whether it is now covered elsewhere, moved to a sibling ticket, or plainly
unclaimed. Deleting the Google 429 fixture, for instance, leaves its status and
`Retry-After` transport coverage unclaimed; that is acceptable only because it
is written down.

## Consequences

This is a program, not a posture change. Honest sizing: a strict replay host
with explicit redrive is 2–3 weeks; parameterising and triaging the affected
suites is 3–6 weeks on top; capture and redaction tooling with an initial
provider corpus is 1–2 weeks. CI capacity is not free — the distributed Restate
lane already carries a 45-minute timeout.

The sequencing follows cost: cheap invariants first because they generalise
furthest, one caller-shaped emission contract per public side-effecting path,
then a small table-driven live-to-replay redrive suite rather than a wholesale
host conversion. Existing suites are not rewritten wholesale, and ordinary unit
tests are kept unless they mechanically duplicate the implementation.

A simulation or CI failure is a finding until an RCA proves otherwise. "Flaky"
is a conclusion requiring evidence and a filed artifact, not a rerun.
ADR 0008 makes that operational: the first failing attempt's artifact is
immutable evidence, and any retry-to-green or quarantine must carry a
machine-checked owner, issue, RCA status, and expiry. This ADR governs what
constitutes credible evidence; ADR 0008 governs how CI retains and disposes of
that evidence.
