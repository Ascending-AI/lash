# E2E Scenario: Graceful Drain

> **Read [../RULES.md](../RULES.md) first.** This is the agent-judged semantic layer over
> the deterministic `just graceful-drain-e2e` companion. Do not replace the companion's
> process assertions with manual database writes, and do not treat a green script as the
> judgment itself.

**Purpose.** Prove the published graceful-drain procedure is the behavior lash actually
has. The host stops admission, lets an already-admitted effect finish, parks the session,
closes the provider and flushes tracing, and invokes
`DurableProcessWorker::drain_owner_bound_work` only after process run leases are released.
The judgment compares every drain claim in `docs/operations.html` with the persistent
PostgreSQL deployment the companion observed.

**Deterministic companion.** Run with a fresh artifact directory:

```sh
LASH_GRACEFUL_DRAIN_ARTIFACT_DIR=<fresh-dir> just graceful-drain-e2e
```

The companion owns one PostgreSQL 16 container named
`lash-fig897-graceful-drain-postgres`, published on loopback port `5547` by default
(`LASH_GRACEFUL_DRAIN_POSTGRES_PORT` overrides it). It acquires the worktree gate, labels
the container with the worktree identity, and removes the container on success or failure.
It emits `graceful-drain e2e passed: scenarios=1` only after the contract test, docs lint,
live scenario, and artifact assertions pass.

**No real tokens.** The in-flight turn uses a deterministic provider that parks inside one
real LLM effect until the host releases it, then returns a fixed terminal cell in the row's
own dialect (`LASH_RUNBOOK_DIALECT`, as RULES.md requires). Do not configure a live
provider.

**Two layers, and the dialect belongs to both.** The scripted layer is the companion's
terminal cell: it is not judged for language quality, but it must be a cell the session can
*execute*, because a foreign cell never commits and the turn then never reaches a terminal
state — the row hangs rather than failing. The judged layer is everything above it: the
drain procedure, the persisted dispositions, and the docs comparison, none of which differ
by dialect. So the two dialect rows of this scenario differ in exactly one observable, the
language of the committed cell, and agree on every drain claim. Confirm the served dialect
from the row's own evidence (the committed cell's tag), never from the environment
variable you set.

**Fixture honesty.** The controller-owned journal in this scenario is an in-process ledger
of active replay keys, not a claim that `InlineEffectHost` has a durable workflow journal.
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
5. **Docs claims are assertions.** A documented claim with no companion evidence is a
   finding. Any observed contradiction is a real-defect stop; do not weaken this runbook.
6. **A commit-budget rejection is terminal.** The host must supply explicit byte and node
   policy. If turn settlement or `park()` reports the typed byte/node rejection, do not
   retry the identical operation: raise the configured limit or make the commit smaller.
   The 1 MiB / 512-node pair used by first-party hosts is a recommended starting point,
   not Lash-owned authority.

## Working material

- Companion artifacts from the command above. `03-observed.jsonl` is backend truth.
- Docs surface: serve checked-in `docs/` on an unused loopback port and open
  `/operations.html#graceful-drain`.
- Source truth: `crates/lash-core/src/runtime/process_worker/mod.rs` and
  `crates/lash/src/process_admin.rs`; the budget error contract is covered by
  `crates/lash/src/tests/core_session_builder/session_lifecycle.rs`.
- Save rendered section text, the named screenshot, and the completed scorecard in the
  artifact directory. Do not edit docs or sources during judgment.

## Phase 0 — Contract and deployment gates

Require:

- `00-container.json` names the owned PostgreSQL container and `00-postgres.json` reports
  the selected loopback port;
- `01-contract-tests.log` passes the facade owner-drain end-to-end test;
- `02-docs-lint.log` is green; and
- the companion later removes its container.

**Fail if:** the container uses a name outside the task's ownership prefix, publishes a
port outside `5540-5599`, the focused test or docs lint fails, or the run leaves a container
behind.

## Phase 1 — Seed an honestly in-flight deployment

Read `seeded_drain_deployment` in `03-observed.jsonl`.

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

## Phase 2 — Execute the documented drain order

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

Compare the final `processes` array to the documented verdicts.

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

## Phase 4 — Score the docs against the observed run

Serve `docs/` on loopback and open `/operations.html#graceful-drain`. Poll until
**Graceful Drain** and **Background Process Recovery** render. Save the Graceful Drain
section text as `04-docs-claims.txt` and capture `04-graceful-drain.png` with the six-step
procedure and worker-specific paragraph visible.

| Documented claim | Evidence |
|---|---|
| Admission and grace-budget policy belong to the host | `graceful_drain_observed` ingress fields |
| An admitted turn may finish before shutdown | completed turn and controller journal fields |
| Parking flushes and releases an idle session | `parked_session_id` after turn completion |
| Provider close and trace flush are explicit ordered levers | `provider_closed`, `trace_flushed` |
| Worker drain is separate from facade/session drain | explicit `drain_report_abandoned` and empty `drain_report_deferred` after park |
| This worker's started OwnerBound rows become `Abandoned{OwnerDrain}` | final mine row and observer terminal |
| Rerunnable work receives no terminal | final rerunnable row |
| Other-owner, unstarted OwnerBound, and ExternallyOwned rows remain untouched | final process array |

A page step the companion did not perform, or a companion-observed required step absent
from the page, is a docs/behavior contract violation. Stop and report it as a finding.

## Phase 5 — Teardown and score

Require `panic gate: clean`, `graceful-drain e2e passed: scenarios=1`, a closed docs port,
and no `lash-fig897-graceful-drain-postgres` container.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Contract coverage | focused facade test and docs lint green | | `01-contract-tests.log`, `02-docs-lint.log` |
| Honest in-flight seed | parked provider call and a non-empty active journal | | `03-observed.jsonl` seed checkpoint |
| Quiesced ingress | admission closed and second turn rejected | | `03-observed.jsonl` observed checkpoint |
| Effect settlement | admitted turn committed; active journal empty; completed key retained | | `03-observed.jsonl` |
| Session/provider/trace shutdown | session parked, provider closed, trace flushed | | `03-observed.jsonl` |
| Owner-bound drain | exact mine id is `Abandoned{OwnerDrain}` and observer agrees | | `03-observed.jsonl` |
| Untouched work | rerunnable, foreign, unstarted, and external rows remain non-terminal | | `03-observed.jsonl` |
| Docs agreement | every scored claim matched observed evidence | | `04-docs-claims.txt`, `04-graceful-drain.png` |
| Teardown | panic gate clean; owned container and docs port gone | | `graceful-drain-e2e.log`, container inventory |

**Aggregate:** would a host following only the published procedure stop admission, settle
its admitted effects, and write exactly the process terminals it owns without stranding or
misclassifying any other work?

---

_Stop triggers and the Abort/RCA protocol are in [../RULES.md](../RULES.md). A docs versus
behavior divergence is a product finding: preserve artifacts and stop._
