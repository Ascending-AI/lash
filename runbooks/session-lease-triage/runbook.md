# E2E Scenario: Triaging A Stuck Turn

> **Read [../RULES.md](../RULES.md) first.** This is the agent-judged semantic layer over
> the deterministic `just session-lease-triage-e2e` companion. Do not replace the
> companion's assertions with manual store reads, and do not treat a green script as the
> judgment itself.

**Purpose.** Prove that the published stuck-turn triage procedure is the behavior lash
actually has. A turn that stops producing output has three causes that look identical from
outside (a hanging provider, a lease that moved to another worker, two writers livelocked
on one session head), and the operations page claims two surfaces distinguish them: the
`session_lease_diagnostics` snapshot and the four `session_execution_lease.*` trace events.
The judgment is a comparison: every reading and every event the page promises must appear,
carry the fields the page names, and mean what the page says it means.

**Deterministic companion.** Run with a fresh artifact directory:

```sh
LASH_SESSION_LEASE_ARTIFACT_DIR=<fresh-dir> just session-lease-triage-e2e
```

The companion owns no container and no host port, so it never serializes against another
worktree's assigned PostgreSQL service. It runs every phase on SQLite always, and on
PostgreSQL as well when `LASH_POSTGRES_DATABASE_URL` names one. Session ids carry a
per-run suffix (a session id is single-use, ADR 0049), so a shared database needs no
truncation and repeated runs never collide. It emits
`session-lease-triage e2e passed: scenarios=3` only after every phase assertion holds on
every configured backend. Its `0*`-prefixed artifacts are the truth for this judged
runbook.

**No real tokens.** Turns run against a deterministic in-process provider returning one
fixed Lashlang program, plus a variant of it that parks forever. Do not configure a live
provider for this scenario.

**Fixture honesty.** The parked turn is a provider that never returns, which is the real
shape of a provider hang with no timeout. The takeover is staged as an *abandoned lease
row*: a TTL-zero claim made straight through the store, so no guard and no renewal task
exist behind it, which is what a killed or frozen worker leaves behind. Nothing releases the
lane on the dead worker's behalf and nothing waits for it to notice, because it never will.
That absence is the scenario, not a shortcut around it.

Emitter liveness is exactly what separates the two loser shapes, and the harness deliberately
tests the harder one. A *live* holder that loses its lane additionally logs its own
`session_execution_lease.lost`; a dead one logs nothing at all. Those event sets are **not** identical, and a
takeover reported from the loser would be absent in the dead case. The live-loser variant is
covered by the `lash-core` unit tests (`session_lease_observability`); this runbook covers
the case that used to go unreported.

## Scenario-specific golden rules

1. **A healthy lane is evidence, and its evidence is silence.** The provider-hang phase
   must produce a `Current` reading naming the parked worker and produce **no**
   `session_execution_lease.lost`, `taken_over`, or `commit_cas_rejected` event. A run that finds lease
   trouble around a hanging provider has not isolated the provider.
2. **The winner reports the takeover, and it reports the truth.** `taken_over` must be
   emitted by the worker that claimed the lane, name the abandoned holder as
   `displaced_owner_id`/`displaced_fencing_token`, and carry a strictly higher `generation` of
   its own. The dead holder must emit **zero** events: a run that finds a `session_execution_lease.lost`
   here is not testing a dead loser and its takeover evidence proves nothing about the case
   under review.
3. **A lost lease is not a failed turn.** The displaced turn's fate is recorded, not
   assumed. If it committed, no `commit_cas_rejected` may exist for it; if it failed, the
   error is captured. A run that treats lease loss as proof of failure contradicts the
   contract the docs stake the "do not kill it" instruction on.
4. **Livelock is recurrence, not one collision.** Every round of sustained misrouting must
   produce a rejection carrying `lease_lost = false`, `lane_held = false`, and a head revision
   that moved on, with no `taken_over` in the timeline. A single rejection is ordinary
   concurrent-writer contention and the operations page says so separately; a run that shows
   one collision has not evidenced the diagnosis that prescribes an identity fix.
5. **Diagnostics never authorize an action.** No phase may use the reading to fence, cancel,
   or kill anything. If a step needs the lease to decide behavior, the step is wrong.
6. **Docs claims are assertions.** Each documented statement about triage is scored against
   an artifact. A claim with no evidence behind it is a finding against the docs, not a pass
   by default.

## Working material

- Companion command and artifacts, from the repository root, as above.
- Docs surface: serve the checked-in `docs/` directory on an unused loopback port, open
  `/operations.html`, and stop the server during teardown. The server only exposes static
  in-repo files.
- Source truth for the surfaces under test:
  `crates/lash-core/src/runtime/session_execution_lease.rs` (the four events),
  `crates/lash/src/session_lease.rs` (the reading), and
  `examples/agent-service/src/lease_triage.rs` (the operator endpoint).
- Save command output and rendered text in the run artifact directory. Do not edit docs or
  sources during a judged run; a divergence is a finding.

## Phase 0 — Contract coverage

**Setup.** `00-trace-event-tests.log` and `01-facade-read-tests.log` are the unit gates the
companion runs before it stages anything.

**Action.** Confirm both suites passed, and that `05-docs-lint.log` reports the docs lint
green (the documented procedure's Rust block is compiled from `examples/docs-snippets` and
must still match it).

**Expected observable evidence.** The lease-event suite covers all four transitions
including the negative case (a renewal that failed while the row still names this owner is
not reported as a takeover). The facade suite covers absent, unheld, current, and lapsed
readings plus the no-disturbance property.

**Fail if:** either suite is absent or failing, the docs lint is not green, or the run's
`backends` line names a backend no phase reported on.

## Phase 1 — Provider hang: the lane is healthy

**Setup.** `02-provider-hang.jsonl`, one checkpoint per backend. A real turn is parked
inside a provider call that never returns, and a second `LashCore` sharing only the durable
store reads the lane, which is the operator's vantage point rather than the running
worker's.

**Action.** Read `reading_while_parked`, the `session_execution_lease.acquired` event, the
three lease-trouble counters, and `reading_after_commit`.

**Expected observable evidence.** `claimed` is `INFO` and carries session id, generation,
owner id, and incarnation id. The parked reading is `current` with positive
`expires_in_ms`, and its `holder_owner_id` is the worker that owns the parked turn. A
renewal landed before the reading was taken, so `current` reflects a live renewal loop
rather than the original claim's headroom. `lease_lost_count`, `taken_over_count`, and
`commit_cas_rejected_count` are all `0`. Releasing the provider commits the turn, after
which the lane reads `unheld`.

**Judgment — FAIL if:** the reading names a different holder, reports `lapsed` or `unheld`
while the turn is provably in flight, carries no headroom, or any lease-trouble event fired.
A reading that cannot be taken at all while a turn holds the lane is the sharpest possible
failure: the whole point is that triage is free to run against a live session.

## Phase 2 — Takeover: a dead worker's lane, swept by a real turn

**Setup.** `03-lease-takeover.jsonl`. One committed turn materializes the session, then an
abandoned lease row is seeded: TTL zero, claimed through the store, no guard and no renewal
task behind it. A real turn then claims the session.

**Action.** Read the `taken_over` event with every field, the `session_execution_lease.lost` count, the
readings either side of the sweep, and the sweeping turn's recorded fate.

**Expected observable evidence.** Exactly one `taken_over`, at `INFO`, emitted by the
successor: its own `generation`/`owner_id`/`incarnation_id` are the winner's, and
`displaced_owner_id`/`displaced_fencing_token` name the abandoned holder exactly, strictly
below the winner's generation. `lease_lost_count` is `0`, because the abandoned holder runs
nothing. The pre-sweep reading names the abandoned holder as `lapsed`; the post-sweep reading
names the successor at a higher generation. The sweeping turn then settles, and the run
records which way.

**Judgment — FAIL if:** no `taken_over` appears, it is emitted by anyone but the winner, it
names the wrong displaced holder or generation, the generation did not advance, a claim
reports displacing itself, the pre-sweep reading is not a lapsed row naming the abandoned
holder, the operator read still shows the old holder afterwards, or the turn neither
committed nor reported an error. A `session_execution_lease.lost` in this phase is also a failure: it means
the loser was alive, so the run silently substituted the easy case for the one under test.

## Phase 3 — Livelock: sustained misrouting, repeated rejections

**Setup.** `04-commit-cas-livelock.jsonl`. Three rounds of the misconfiguration the docs
name: two runtime opens are handed the same session under one explicit core worker identity.
Their owner id and boot incarnation match, but their runtime-minted executor ids differ. The
second claim is therefore Busy rather than reentry; the busy claimant remains lane-less, both
run a turn at once, and the head CAS alone selects the winner. Each round is a fresh pair,
which is what a retry-on-conflict host does after losing.

**Action.** Read `rounds_attempted`, `rounds_with_a_rejection`, the per-round records, and
every `commit_cas_rejected` event.

**Expected observable evidence.** Every round has exactly one winner and one rejected
commit, so `rounds_with_a_rejection` equals `rounds_attempted` and the rejection count is at
least one per round. Each rejection is `WARN` and carries session id, owner id, incarnation
id, executor id, `lease_lost = false`, `lane_held = false`, and an `actual_head_revision`
strictly above `expected_head_revision`. No `session_execution_lease.lost` and no `taken_over` appear, so the
situation is unambiguously a recurring race rather than a handoff.

**Judgment — FAIL if:** any round has zero or two winners, any round produces no rejection
(then the misrouting is not actually recurring and the run has proved contention, not
livelock), a rejection reports `lease_lost = true` or `lane_held = true`, the head revisions
do not show the head moving on, or a handoff event appears alongside.

## Phase 4 — Score the documented procedure against the observed run

Serve `docs/` on loopback and open `/operations.html`. Poll until the **Triaging A Stuck
Turn** section renders, then score each claim below against the named artifact. Save the
rendered section text as `06-docs-claims.txt`.

| Documented claim | Evidence |
|---|---|
| The read is a snapshot that never claims, renews, or releases | `02-provider-hang.jsonl` (repeated reads against a live holder), `01-facade-read-tests.log` |
| An absent session reads differently from an unheld lane | `01-facade-read-tests.log` |
| Every lease event carries session id and the applicable owner, incarnation, and executor identities | all three phase artifacts |
| The four events sit at the levels the table names | `acquired`/`taken_over` INFO, `lost`/`commit_cas_rejected` WARN, in all three artifacts |
| `Current` with no `session_execution_lease.lost` means the turn is blocked inside itself | `02-provider-hang.jsonl` |
| The winner reports `taken_over` naming the holder it displaced, even when that holder is dead | `03-lease-takeover.jsonl` (`lease_lost_count` is 0) |
| A lost lease does not mean the turn failed, so do not kill the runner | `03-lease-takeover.jsonl` (`turn_committed_after_takeover`) |
| One rejection is contention; *repeated* rejections with `lease_lost = false` are livelock, and the fix is worker identity | `04-commit-cas-livelock.jsonl` (per-round records) |
| Only `commit_cas_rejected` proves a turn did not publish | `03-lease-takeover.jsonl` versus `04-commit-cas-livelock.jsonl` |
| Lease churn is trace telemetry, not durable session history | absence of any lease entry in session events; the events exist only in the captured timeline |

A page that promises a reading the companion never produced, or a companion observation the
page omits, is a **contract violation** between docs and behavior: report it as a finding.

The livelock row's *cause* is only partly observable here: the harness stages the shared host
identity directly rather than routing two host requests into it, so it evidences distinct
per-open executors, Busy/lane-less publication, recurrence, and a rejected commit each round,
while leaving the host-routing half to the deployment. Identical owner/incarnation/executor
triples would indicate unintended reentry and must fail the scorecard.

## Phase 5 — Teardown and score

Stop the static docs server and confirm its loopback port is closed. Require the companion's
final `panic gate: clean` and `session-lease-triage e2e passed: scenarios=3` lines, and
confirm no container or host port was left behind (the companion owns none).

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Contract coverage | four-event suite and facade-read suite green; docs lint green | | `00-trace-event-tests.log`, `01-facade-read-tests.log`, `05-docs-lint.log` |
| Provider hang | `current` reading naming the parked worker, positive headroom, zero lease-trouble events | | `02-provider-hang.jsonl` |
| Lease release on commit | the committed turn's lane reads `unheld` | | `02-provider-hang.jsonl` |
| Winner-emitted takeover | one `taken_over` from the winner naming the abandoned holder and generation | | `03-lease-takeover.jsonl` |
| Dead loser stays silent | `lease_lost_count` is 0, so the event does not depend on loser liveness | | `03-lease-takeover.jsonl` |
| Lease loss is not failure | the sweeping turn's fate recorded and self-consistent | | `03-lease-takeover.jsonl` |
| CAS livelock recurs | every round: one commit, one rejection with `lease_lost = false` and `lane_held = false` from a different executor under the same host owner | | `04-commit-cas-livelock.jsonl` |
| Backend agreement | every phase reported the same verdicts on each configured backend | | all phase artifacts |
| Docs agreement | every scored claim matched an artifact | | `06-docs-claims.txt` |
| Teardown | panic gate clean; no owned containers or ports remain | | `session-lease-triage-e2e.log` |

**Aggregate:** would an operator who followed only the published procedure have reached the
right conclusion in all three situations, and would that operator have been correctly
stopped from killing a worker whose turn was about to commit?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md). A
failing live scenario is a product finding: preserve the artifact directory and stop; never
loosen an assertion or rewrite the judgment criterion during that run._
