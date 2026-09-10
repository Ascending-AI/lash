# E2E Scenario: Graceful Drain

> **Read [../RULES.md](../RULES.md) first.** This is the agent-judged semantic layer over
> the deterministic `just graceful-drain-e2e` companion. Do not replace the companion's
> process assertions with manual database writes, and do not treat a green script as the
> judgment itself.

**Purpose.** Prove that this scenario's graceful-drain procedure composes Lash's host-owned
shutdown levers without losing admitted work or inventing process outcomes. This fixture's
host policy stops admission, lets an already-admitted effect finish, parks the session,
releases process run leases before calling
`DurableProcessWorker::drain_owner_bound_work`, then closes the provider and flushes tracing.
That total order belongs to this fixture; Lash requires the capability dependencies and
leaves the rest of the ordering, grace budget, and cancel-versus-await choice to each host.

**Deterministic companion.** Run with a fresh artifact directory:

```sh
LASH_GRACEFUL_DRAIN_ARTIFACT_DIR=<fresh-dir> just graceful-drain-e2e
```

The companion owns one PostgreSQL 16 container named
`lash-fig897-graceful-drain-postgres`, published on loopback port `5547` by default
(`LASH_GRACEFUL_DRAIN_POSTGRES_PORT` overrides it). It acquires the worktree gate, labels
the container with the worktree identity, and removes the container on success or failure.
It emits `graceful-drain e2e passed: scenarios=1` only after the contract test, live
scenario, and artifact assertions pass.

**No real tokens.** The in-flight turn uses a deterministic provider that parks inside one
real LLM effect until the host releases it, then returns a fixed terminal cell in the row's
own dialect (`LASH_RUNBOOK_DIALECT`, as RULES.md requires). Do not configure a live
provider.

**Two layers, and the dialect belongs to both.** The scripted layer is the companion's
terminal cell: it is not judged for language quality, but it must be a cell the session can
*execute*, because a foreign cell never commits and the turn then never reaches a terminal
state — the row hangs rather than failing. The judged layer is everything above it: the
drain procedure, the persisted dispositions, and the observed-behavior judgment, none of
which differ by dialect. So the two dialect rows of this scenario differ in exactly one observable, the
language of the committed cell, and agree on every drain claim. Confirm the served dialect
from the row's own evidence (the committed cell's tag), never from the environment
variable you set.

**Fixture honesty.** The controller-owned journal in this scenario is an in-process ledger
of active replay keys, not a claim that `NativeEffectHost` has a durable workflow journal.
It proves the host waited for the exact admitted effect before declaring its journal empty.
The workflow-engine persistence half belongs to the Restate runbooks. Process rows use
inert `External` inputs because execution is not under test; drain reads only the persisted
disposition, `first_started` owner, lease availability, and terminal facts. The scenario
does not fabricate a terminal or call a registry completion method.

## Scenario-specific golden rules

1. **Admission closes before settlement.** The parked effect must already be in the journal
   before ingress closes. A second turn is rejected by the host edge, while the admitted
   turn is allowed to finish.
2. **The journal must actually drain.** The seed checkpoint must name an active replay key;
   the final checkpoint must have no active keys and at least one completed key. An empty
   ledger that never held work proves nothing.
3. **OwnerDrain is exact.** Only the started `OwnerBound` row whose `first_started.owner`
   equals the worker's stable owner becomes `Abandoned{OwnerDrain}`. Its evidence must name
   that owner, and a held `await_output` must resolve to the same terminal.
4. **Opposite dispositions remain opposite.** A started `Rerunnable` row remains
   non-terminal. The foreign-owner, never-started `OwnerBound`, and `ExternallyOwned` rows
   are also untouched.
5. **Every claim needs observed evidence.** A required outcome with no companion evidence is
   a finding. Any observed contradiction is a real-defect stop; do not weaken this runbook.
6. **A commit-budget rejection is terminal.** The host must supply explicit byte and node
   policy. If turn settlement or `park()` reports the typed byte/node rejection, do not
   retry the identical operation: raise the configured limit or make the commit smaller.
   The 1 MiB / 512-node pair used by first-party hosts is a recommended starting point,
   not Lash-owned authority.

## Evidence to inspect

- Companion artifacts from the command above. `03-observed.jsonl` is backend truth.
- The procedure and expected operator decisions are in this runbook. The companion artifacts
  are the independent behavior evidence; do not score the prose by reading the prose again.
- Save the completed scorecard in the artifact directory. Do not edit the runbook, companion,
  or artifacts during judgment.

## Phase 0 — Contract and deployment gates

Require:

- `00-container.json` names the owned PostgreSQL container and `00-postgres.json` reports
  the selected loopback port;
- `01-contract-tests.log` passes the facade owner-drain end-to-end test; and
- the companion later removes its container.

**Fail if:** the container uses a name outside the task's ownership prefix, publishes a
port outside `5540-5599`, the focused test fails, or the run leaves a container
behind.

## Phase 1 — Seed an honestly in-flight deployment

Read `seeded_drain_deployment` in `03-observed.jsonl`. Use its emitted
`seeded_session_id` when checking the parked session against the seed; the
`in_flight_turn_id` is the separate turn identity for the admitted effect.

Require exactly one provider call parked in flight, a non-empty `journal_active`, ingress
still accepting, and five non-terminal process rows:

- this worker's started `OwnerBound` row;
- this worker's started `Rerunnable` row;
- another worker's started `OwnerBound` row;
- a never-started `OwnerBound` row; and
- an `ExternallyOwned` row.

**Fail if:** the effect already finished, the journal was never populated, any row is
terminal before drain, or the seeded ownership/disposition facts do not distinguish all
five verdicts.

## Phase 2 — Execute this fixture's drain order

The companion performs the host-owned sequence: close admission; reject a newly offered
turn; release and await the already-admitted effect; park its session; confirm no active
journal entries; call `drain_owner_bound_work`; close the provider; flush the trace sink.

Read `graceful_drain_observed` and require:

- `ingress_accepting` and `new_turn_admitted` are both false;
- `in_flight_effect_completed` is true with terminal value `drained`;
- `journal_active` is empty and `journal_completed` is non-empty;
- the parked session id equals the seeded session;
- provider close and trace flush completed; and
- `drain_report_abandoned` contains only `drain-owner-bound-mine`; and
- `drain_report_deferred` is empty (including no peer-settled or backend-error row); and
- `drain_worker_faults` is zero — the companion wires a `ProcessEventSink` and records every
  `ProcessWorkerFault` the worker reports, so a fault stranded after admission cannot hide
  behind a clean-looking drain.

**Fail if:** the host admits work after quiesce, drops the in-flight effect, parks before it
commits, declares an empty journal without a completed key, or invokes a substitute
terminal-writing path.

## Phase 3 — Judge disposition and ownership outcomes

Compare the final `processes` array to the required verdicts.

| Process row | Required final fact |
|---|---|
| `drain-owner-bound-mine` | terminal `Abandoned`, writer `OwnerDrain`, owner `drain-host` |
| `drain-rerunnable-mine` | non-terminal `Running` |
| `drain-owner-bound-foreign` | non-terminal `Running`, foreign `first_started` owner retained |
| `drain-owner-bound-unstarted` | non-terminal `Running`, no `first_started` |
| `drain-externally-owned` | non-terminal `Running` |

Also require the held observer resolved as `Abandoned{OwnerDrain}` with the same owner.

**Judgment — FAIL if:** any extra row terminalized, the mine row stayed live, the evidence
writer or owner differs, or the observed terminal disagrees with the registry record.

## Phase 4 — Judge the host procedure from observed behavior

Correlate the seeded and final observations in `03-observed.jsonl`; do not accept the
companion's pass line as the judgment. Check the final process array against both its seeded
state and the drain report.

| Required operator conclusion | Independent behavior evidence |
|---|---|
| The seed captured work before drain began | ingress is accepting, one provider call is parked, and the journal has an active replay key |
| The final state rejects admission and has settled the seeded work | ingress and new-turn fields are false; the seeded replay key is completed, no key is active, and the turn returns `drained` |
| The final state parks the seeded session | seeded session id equals the final parked session id |
| Owner drain selected no ineligible or still-held row | drain report has the one eligible id, no deferred id, and the final array leaves every opposite case live |
| Owner drain abandoned exactly its eligible row | seeded five-row ownership/disposition split matches the final array and single report id |
| Provider close and trace flush both completed | final checkpoint carries the terminal result and both completion flags |

Missing fields, inconsistent identities, or a conclusion that requires facts outside the
artifact bundle are failures. Preserve the bundle and report the unsupported claim.

The bundle exposes a seed and a final shutdown checkpoint, not intermediate timestamps. It
therefore does not independently prove when admission closed relative to the active effect,
when settlement occurred relative to the journal becoming empty, when the session parked,
when process run leases were released, or when provider close and trace flush occurred.
Record that limitation; do not promote endpoint correlations or final true flags into
ordering evidence. The host sequence described above remains this fixture's source-selected
policy. The artifacts prove its endpoint outcomes and the observable consequence of the
lease prerequisite: exactly the eligible owned row was abandoned and no row was deferred.

## Phase 5 — Teardown and score

Require `panic gate: clean`, `graceful-drain e2e passed: scenarios=1`, and no
`lash-fig897-graceful-drain-postgres` container.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Contract coverage | focused facade test green | | `01-contract-tests.log` |
| Honest in-flight seed | parked provider call and a non-empty active journal | | `03-observed.jsonl` seed checkpoint |
| Quiesced ingress | admission closed and second turn rejected | | `03-observed.jsonl` observed checkpoint |
| Effect settlement | admitted turn committed; active journal empty; completed key retained | | `03-observed.jsonl` |
| Session/provider/trace shutdown | session parked, provider closed, trace flushed | | `03-observed.jsonl` |
| Owner-bound drain | exact mine id is `Abandoned{OwnerDrain}` and observer agrees | | `03-observed.jsonl` |
| Untouched work | rerunnable, foreign, unstarted, and external rows remain non-terminal | | `03-observed.jsonl` |
| Procedure judgment | every Phase 4 conclusion matched independent observed evidence | | `03-observed.jsonl`, completed scorecard |
| Teardown | panic gate clean; owned container gone | | `graceful-drain-e2e.log`, container inventory |

**Aggregate:** would a host following only this self-contained procedure stop admission, settle
its admitted effects, and write exactly the process terminals it owns without stranding or
misclassifying any other work?

---

_Stop triggers and the Abort/RCA protocol are in [../RULES.md](../RULES.md). An expected
behavior versus observed-artifact divergence is a product finding: preserve artifacts and stop._
