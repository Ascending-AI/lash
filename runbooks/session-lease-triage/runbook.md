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

**Fixture honesty.** Two faults are injected rather than waited for. The parked turn is a
provider that never returns, which is the real shape of a provider hang with no timeout.
The takeover is performed on demand with the peer's own mechanism: a busy claim outcome
carries the lease it lost to by contract, so the harness releases that exact fence and
claims it for a named successor instead of idling out a TTL. Both leave the production
renewal and commit paths untouched, so the events under judgment are the real ones. What
the harness does *not* reproduce is a worker whose whole process stalls; the events are
identical from the row's point of view, and no claim here depends on the difference.

## Scenario-specific golden rules

1. **A healthy lane is evidence, and its evidence is silence.** The provider-hang phase
   must produce a `Current` reading naming the parked worker and produce **no**
   `renew_failed`, `taken_over`, or `commit_cas_rejected` event. A run that finds lease
   trouble around a hanging provider has not isolated the provider.
2. **Order is the evidence in a takeover.** `renew_failed` must precede `taken_over`, both
   must name the same displaced generation, and `taken_over` must name a strictly higher
   superseding generation under a different owner. Two unordered events do not reconstruct
   a handoff.
3. **A lost lease is not a failed turn.** The displaced turn's fate is recorded, not
   assumed. If it committed, no `commit_cas_rejected` may exist for it; if it failed, the
   error is captured. A run that treats lease loss as proof of failure contradicts the
   contract the docs stake the "do not kill it" instruction on.
4. **Livelock is told apart by `lease_lost`, not by vibes.** The rejected commit must carry
   `lease_lost = false` and name a head revision that moved on, with no `taken_over` in the
   timeline. That combination, and only it, separates two racing writers from an ordinary
   handoff.
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

**Action.** Read `reading_while_parked`, the `session_execution_lease.claimed` event, the
three lease-trouble counters, and `reading_after_commit`.

**Expected observable evidence.** `claimed` is `INFO` and carries session id, generation,
owner id, and incarnation id. The parked reading is `current` with positive
`expires_in_ms`, and its `holder_owner_id` is the worker that owns the parked turn. A
renewal landed before the reading was taken, so `current` reflects a live renewal loop
rather than the original claim's headroom. `renew_failed_count`, `taken_over_count`, and
`commit_cas_rejected_count` are all `0`. Releasing the provider commits the turn, after
which the lane reads `unheld`.

**Judgment — FAIL if:** the reading names a different holder, reports `lapsed` or `unheld`
while the turn is provably in flight, carries no headroom, or any lease-trouble event fired.
A reading that cannot be taken at all while a turn holds the lane is the sharpest possible
failure: the whole point is that triage is free to run against a live session.

## Phase 2 — Takeover: the lane moved, and the turn may still commit

**Setup.** `03-lease-takeover.jsonl`. The same parked turn, with its durable lane swept
mid-turn by a named successor.

**Action.** Read the `renew_failed` and `taken_over` events with every field, the ordering
flag, the readings either side of the sweep, and the displaced turn's recorded fate.

**Expected observable evidence.** `renew_failed` is `WARN`; `taken_over` is `INFO` and adds
`superseding_generation`, `superseding_owner_id`, and `superseding_incarnation_id`.
`renew_failed` precedes `taken_over`, both name the displaced generation, and the
superseding generation is strictly higher under a different owner. The pre-sweep reading
names the displaced holder; the post-sweep reading names the successor at a higher
generation. The displaced turn then settles, and the run records which way.

**Judgment — FAIL if:** either event is missing or omits an identity field, the two are out
of order or disagree about the displaced generation, the successor is reported as its own
predecessor, the generation did not advance, the operator read still shows the old holder
afterwards, or the turn neither committed nor reported an error. A run in which the
displaced turn committed is not a failure; it is the contract, and the scorecard says so.

## Phase 3 — Livelock: two writers, one head

**Setup.** `04-commit-cas-livelock.jsonl`. Two writers open the same session under one
explicit `session_execution_owner`, so the second reenters the first's lease instead of
being rejected as busy, then both run a turn at once.

**Action.** Read `winner_committed`, `loser_rejected`, the `commit_cas_rejected` event, and
the handoff counters.

**Expected observable evidence.** Exactly one turn committed. The other failed with a head
revision conflict, and exactly one `commit_cas_rejected` event fired: `WARN`, carrying
session id, generation, owner id, incarnation id, `lease_lost = false`, and an
`actual_head_revision` strictly above `expected_head_revision`. No `renew_failed` and no
`taken_over` appear, so the situation is unambiguously a race rather than a handoff.

**Judgment — FAIL if:** both writers committed (the lease was treated as authority and the
CAS did not fence), neither committed, the rejection is missing or reports `lease_lost =
true`, the head revisions do not show the head moving on, or a handoff event appears
alongside. Note that the misconfiguration under test is the one the docs name: a shared
explicit identity. A run that cannot stage it has lost the ability to test the case an
operator will actually hit.

## Phase 4 — Score the documented procedure against the observed run

Serve `docs/` on loopback and open `/operations.html`. Poll until the **Triaging A Stuck
Turn** section renders, then score each claim below against the named artifact. Save the
rendered section text as `06-docs-claims.txt`.

| Documented claim | Evidence |
|---|---|
| The read is a snapshot that never claims, renews, or releases | `02-provider-hang.jsonl` (repeated reads against a live holder), `01-facade-read-tests.log` |
| An absent session reads differently from an unheld lane | `01-facade-read-tests.log` |
| Every lease event carries session id, generation, and holder identity | all three phase artifacts |
| The four events sit at the levels the table names | `claimed`/`taken_over` INFO, `renew_failed`/`commit_cas_rejected` WARN, in all three artifacts |
| `Current` with no `renew_failed` means the turn is blocked inside itself | `02-provider-hang.jsonl` |
| `renew_failed` then `taken_over` orders a handoff and names the successor | `03-lease-takeover.jsonl` |
| A lost lease does not mean the turn failed, so do not kill the runner | `03-lease-takeover.jsonl` (`turn_committed_after_lease_loss`) |
| Repeated rejections with `lease_lost = false` are livelock, and the fix is worker identity | `04-commit-cas-livelock.jsonl` |
| Only `commit_cas_rejected` proves a turn did not publish | `03-lease-takeover.jsonl` versus `04-commit-cas-livelock.jsonl` |
| Lease churn is trace telemetry, not durable session history | absence of any lease entry in session events; the events exist only in the captured timeline |

A page that promises a reading the companion never produced, or a companion observation the
page omits, is a **contract violation** between docs and behavior: report it as a finding.

The livelock row's *cause* is only partly observable here: the harness stages the shared
identity directly rather than routing two host requests into it, so it evidences the
mechanism (reentry plus a rejected commit) and leaves the host-routing half to the
deployment. Say so in the scorecard rather than claiming the cause was reproduced end to
end.

## Phase 5 — Teardown and score

Stop the static docs server and confirm its loopback port is closed. Require the companion's
final `panic gate: clean` and `session-lease-triage e2e passed: scenarios=3` lines, and
confirm no container or host port was left behind (the companion owns none).

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Contract coverage | four-event suite and facade-read suite green; docs lint green | | `00-trace-event-tests.log`, `01-facade-read-tests.log`, `05-docs-lint.log` |
| Provider hang | `current` reading naming the parked worker, positive headroom, zero lease-trouble events | | `02-provider-hang.jsonl` |
| Lease release on commit | the committed turn's lane reads `unheld` | | `02-provider-hang.jsonl` |
| Takeover ordering | `renew_failed` before `taken_over`, same displaced generation | | `03-lease-takeover.jsonl` |
| Named successor | higher superseding generation under a different owner, visible in the read | | `03-lease-takeover.jsonl` |
| Lease loss is not failure | the displaced turn's fate recorded and self-consistent | | `03-lease-takeover.jsonl` |
| CAS livelock | one commit, one rejection with `lease_lost = false` and a moved head | | `04-commit-cas-livelock.jsonl` |
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
