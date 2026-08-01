# Behavior transcripts are one normalized vocabulary

## Status

accepted

## Decision

Every scenario harness that ships an expect test renders into **one shared
behavior-transcript vocabulary**, `lash_core::testing::behavior_transcript`
(re-exported as `lash::testing::behavior_transcript`). The vocabulary is a closed
set of line kinds grouped into four families, a fixed four-column grammar, and a
normalization layer that owns identifier aliasing, free-text scrubbing and size
formatting. Harnesses supply facts; they never supply formatting, and they cannot
supply an unnormalized identifier.

```text
<actor>      <kind>    <event>                 [<key>=<value> ...]
```

```text
root         ingress   turn.start
root         provider  model.request           iteration=0
root         exec      cell.ok                 calls=2
root         outcome   turn.final_value        value={"joined":["left","right"]}
root         commit    checkpoint.commit       rev=0->1
root                     turn_state            stored logical=284B
root                     tool_state            ref (unchanged)
process-001  outcome   process.completed       label="child" kind="lashlang" terminal=true
```

The four families, and the kinds in each:

| Family | Kinds | What a line in it means |
| --- | --- | --- |
| `boundary` | `ingress`, `park`, `resume`, `cancel`, `worker`, `fault` | Execution crossed a durable seam or changed hands |
| `step` | `provider`, `tool`, `exec`, `spawn`, `await`, `wake`, `lease`, `observe` | A causal / ordering event inside a boundary |
| `durable-write` | `commit`, `effect` | Something reached durable storage |
| `terminal` | `outcome` | An actor reached a terminal state |

Adding a family or a kind is a vocabulary change and amends this ADR. Adding an
*event name* within a family (`process.start`, `cell.restore`) does not: event
names are the harness's own vocabulary for what happened, and the family/kind is
what a reviewer scans by.

## Why

`docs/agents/way-of-working.md` already rules on *when* to write an expect test:
when the review artifact is a short deterministic behavior transcript judged as
one diff. `docs/agents/pr-style.md` already enforces the consequence: a durable
line changing in an inline snapshot requires a named `Transcript:` justification,
keyed on `Checkpoint`, `DurableEffect`, `stored logical=`, `ref (unchanged)` and
`rev=`. Both were written for a population of **one** renderer —
`SimulationTrace::render_transcript`, reachable only from `lash-sim` — and one
transcript expect test. The doctrine and the enforcement machinery existed; the
population did not, and the four scenario harnesses of ADR 0007 could not join it
without each inventing a private format.

Three properties decided the shape.

**Deterministic and stable under irrelevant change.** The vocabulary offers no
way to render a wall clock or an elapsed duration, and no way to render a raw
identifier: identifiers enter only through `Attr::id`, which replaces them with a
first-mention alias inside their namespace (`process-001`), and free text goes
through a scrubber that collapses whitespace, masks UUID- and long-hex-shaped
substrings, and truncates. A harness that wants to leak a churning id has to work
at it. There are also **no sequence numbers**: a global counter renumbers the
whole tail when one event is inserted, turning a one-line behavior change into a
whole-artifact diff, so order is carried by line position instead. Column widths
are fixed rather than content-derived for the same reason — one long value widens
its own line and no others.

**Short enough to read as one artifact.** `Transcript::render` panics past a
line budget (80 by default). A harness that genuinely needs more says so with
`with_review_budget`, visibly, in code review. This is the "no decoration" rule
enforced at the cheapest point: a transcript nobody reads is not evidence.

**Expressive enough that a plausible bug changes the text.** The four families
were chosen so the defect classes the repo actually fears each land on a line: a
reordered tool batch moves `tool` lines; a missing checkpoint moves or drops a
`commit` line; a dropped component body flips `stored logical=` to
`ref (unchanged)`; a lost child folds to `process.failed`; a lease supersession is
a `lease` line; an injected backend error is a `fault` line.

**Independence from what it tests (ADR 0044).** The module imports nothing else
from `lash_core`. It consumes strings, integers, booleans and
`serde_json::Value`, so the renderer *cannot* re-derive a fact — the harness has
to extract it from real product state first. This is a compiler-enforced version
of the rule, not a convention. Durable-write lines in particular come from
`lash_core::testing::checkpoint_observer`, a store-factory decorator that records
commits only *after* the backend accepted them.

## Where it lives, and where it does not

`lash_core::testing`, behind the existing `testing` feature that already hosts
the backend conformance suites. Every consumer named in the expect backlog —
`lash-core`'s Runtime Scenarios, `lash-protocol-standard`, `lash-protocol-rlm`,
`lash` (Agent Scenarios), `lash-sim`, the stores, `lash-restate` — sits at or
above `lash-core`, so one module reaches all of them without a new crate on the
release path.

`lash-sansio` sits *below* `lash-core` and therefore cannot use it. If a sans-io
message-rendering expect test is wanted (the prompt/history item in the expect
backlog), promoting this module to a dependency-free crate is the move; it takes
no lash types today precisely so that stays cheap.

The Standard and RLM harnesses both drive a sans-io `TurnMachine`, so the
projection from an `Effect` stream onto the vocabulary is shared once more in
`lash_core::testing::sansio_transcript` rather than written twice.

## Consequences

- `SimulationTrace::render_transcript` keeps its signature and now renders through
  the vocabulary. The simulator owns run-stable actor aliases already, so it pins
  them and the vocabulary does not re-normalize. Two cosmetic changes fall out:
  the actor column is always present (the old renderer dropped it for
  single-session renders, which meant two grammars), and turn changes are an
  attribute rather than a header line.
- `scripts/check-transcript-diff.py` keys on the vocabulary's durable event names
  (`checkpoint.commit`, `checkpoint.request`, `durable.effect`) as well as the
  component renderings and `rev=`. The legacy `Checkpoint` / `DurableEffect`
  tokens stay listed so nothing already blessed silently stops being flagged. The
  Rust-side list is `behavior_transcript::DURABLE_WRITE_EVENTS`; the two are kept
  in sync by hand, and a durable event name the script does not know about is a
  durable-semantics change that can land unremarked.
- Every new transcript expect test carries mutation evidence in the same change
  that blesses it, per ADR 0044's deletion/blessing rule. A snapshot without a
  demonstrated plausible-bug mutation is decoration and does not land.
- Pure invariants stay assertions and property laws. A transcript is added
  *alongside* a scenario's existing assertions, never in place of them: the
  assertions own the contract, the transcript owns the shape a reviewer judges.

## Contradicts nothing; extends

ADR 0007 (four-layer scenario harnesses) keeps its ownership boundaries and its
code-owned coverage indexes untouched. This ADR only says that when one of those
four harnesses renders a behavior transcript, it renders into this vocabulary.
