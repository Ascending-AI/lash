# E2E Scenario: Request Abandon

> **Read [../RULES.md](../RULES.md) first.** This is the agent-judged semantic layer over
> the deterministic `just request-abandon-e2e` companion. Do not replace the companion's
> assertions with manual terminal writes, and do not treat a green script as the judgment.

**Purpose.** Prove the stuck-process escape hatch documented in
`docs/operations.html`: `core.processes().request_abandon` writes a visible durable marker
without terminalizing or disturbing a live owner's lease; natural lease lapse still writes
nothing; and the next durable-process-worker sweep reconciles the authorization into
`Abandoned{ReconciledRequest}` visible to observers.

**Deterministic companion.** Run with a fresh artifact directory:

```sh
LASH_REQUEST_ABANDON_ARTIFACT_DIR=<fresh-dir> just request-abandon-e2e
```

The companion owns one PostgreSQL 16 container named
`lash-fig897-request-abandon-postgres`, on loopback port `5548` by default
(`LASH_REQUEST_ABANDON_POSTGRES_PORT` overrides it). It acquires the worktree gate, labels
the container with the worktree identity, and removes it on every exit. It emits
`request-abandon e2e passed: scenarios=1` only after all exact gates pass.

**No real tokens.** No model call is made. The core carries a deterministic provider only
because it is a complete embeddable runtime.

**Fixture honesty.** The started process uses an inert `External` input stamped
`OwnerBound`; execution is not under test. The scenario exercises the real PostgreSQL
registry, public `core.processes().request_abandon` facade, observer lens, naturally
expiring database-clock lease, public worker sweep, and `await_output`. It never releases
the owner lease to simulate expiry and never writes the terminal directly.

## Scenario-specific golden rules

1. **The request is a marker, not a terminal.** The exact facade return and an independent
   observer read must both remain `Running` and expose who, when, and why.
2. **Authorization is not fencing.** Lease token, fencing token, expiry, and owner identity
   must be byte-for-byte unchanged immediately after the request.
3. **Expiry still proves nothing.** Before the sweep, the lease must be observably lapsed
   while the row remains non-terminal with its marker.
4. **Only the sweep reconciles.** The public durable-process worker must produce
   `Abandoned{ReconciledRequest}`, name the lapsed owner, clear its reconciliation lease,
   and resolve `await_output`.
5. **Observers see both sides.** The seeded observer edge must expose the pending marker
   and the final terminal. A host-wide `get` alone does not satisfy this scenario.
6. **Docs claims are assertions.** Any mismatch is a real-defect stop; never loosen the
   lease timing or terminal checks to make a run pass.

## Working material

- Companion artifacts from the command above; `03-observed.jsonl` is backend truth.
- Docs surface: serve checked-in `docs/` and open `/operations.html#stuck-process`.
- Source truth: `crates/lash/src/process_admin.rs` and
  `crates/lash-core/src/runtime/process_worker/mod.rs`.
- Save rendered text, screenshot, and completed scorecard in the artifact directory.

## Phase 0 — Contract and deployment gates

Require the owned PostgreSQL identity in `00-*`, the facade end-to-end contract test green
in `01-contract-tests.log`, docs lint green in `02-docs-lint.log`, and no container after
the companion exits.

**Fail if:** the container is outside the `lash-fig897-*` ownership prefix, publishes
outside `5540-5549`, a prerequisite gate fails, or teardown leaks it.

## Phase 1 — Seed a live owner and observer

Read `seeded_request_abandon_deployment`. Require process
`request-abandon-owner-bound` to be non-terminal `Running`, its live lease holder to be
`request-abandon-live-owner`, a positive token and fencing token, a future expiry, and an
observer edge for `request-abandon-observer`.

**Fail if:** no live lease exists, the row lacks `first_started`, the lease is already
lapsed, or the process is not visible through the seeded observer lens.

## Phase 2 — Request abandonment while the owner is live

Read `pending_abandon_request_visible`. Require:

- the exact facade return is non-terminal `Running`;
- the marker says `requested_by=runbook-operator`, carries a timestamp, and preserves the
  exact reason;
- an independent `list_observed_by` read sees the same marker; and
- lease owner, lease token, fencing token, and expiry exactly equal Phase 1.

**Judgment — FAIL if:** the facade terminalized the row, the marker is missing from either
read, any lease field changed, or the request touched owner resources.

## Phase 3 — Let the lease lapse, then sweep

The companion polls the persisted lease until its recorded expiry is in the past. It does
not release it. The pre-sweep observation must still be non-terminal `Running`; only then
does it invoke `DurableProcessWorker::drive_pending_processes`.

Read `abandon_request_reconciled` and require:

- `lapsed_before_sweep_status=Running` and `lapsed_before_sweep_terminal=false`;
- final status is terminal `Abandoned`;
- evidence writer is `ReconciledRequest` and names `request-abandon-live-owner`;
- the observer lens sees the terminal;
- `await_output` returned the same evidence;
- the sweep's fenced completion cleared the process lease; and
- `sweep_admitted` is at least one and `sweep_worker_faults` is zero — the drive is an
  admission call, so the companion wires a `ProcessEventSink` and requires the worker to
  report no `ProcessWorkerFault` while reaching that terminal.

**Judgment — FAIL if:** wall-clock expiry terminalizes the row, the worker re-executes the
OwnerBound input, reconciliation occurs before expiry, the evidence names a different
writer/owner, or the observer and registry disagree.

## Phase 4 — Score the docs against the observed run

Serve `docs/` on loopback and open `/operations.html#stuck-process`. Poll until
**Detecting A Stuck Process** renders. Save that section as `04-docs-claims.txt` and
capture `04-request-abandon.png` with the classification recipe and escape-hatch text
visible.

| Documented claim | Evidence |
|---|---|
| Stuckness is host classification over raw facts | seeded and pending lease fields |
| A started OwnerBound row stays non-terminal after lease lapse | pre-sweep fields in reconciled checkpoint |
| `request_abandon` records who, when, and why | pending checkpoint |
| The marker is returned and observer-visible while pending | pending checkpoint facade and observer gates |
| The request never terminalizes or fences the owner | pending lifecycle and exact lease equality |
| Only a post-lapse sweep produces `Abandoned{ReconciledRequest}` | lapsed pre-sweep and final fields |
| The final is observable and resolves terminal waiting | final observer and `await_output` gates |

A page promise the companion did not observe, or required companion behavior the page
omits, is a docs/behavior contract violation. Preserve evidence and stop.

## Phase 5 — Teardown and score

Require `panic gate: clean`, `request-abandon e2e passed: scenarios=1`, a closed docs
port, and no `lash-fig897-request-abandon-postgres` container.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Contract coverage | focused facade test and docs lint green | | `01-contract-tests.log`, `02-docs-lint.log` |
| Live owner seed | running OwnerBound row with live named lease and observer | | `03-observed.jsonl` seed checkpoint |
| Pending marker | facade return and observer show who/when/why, still Running | | `03-observed.jsonl` pending checkpoint |
| Lease preserved | owner, token, fence, and expiry exactly unchanged | | seed versus pending checkpoints |
| Expiry is non-terminal | lapsed lease observed before sweep while row is Running | | reconciled checkpoint pre-sweep fields |
| Sweep reconciliation | terminal `Abandoned{ReconciledRequest}` names lapsed owner | | reconciled checkpoint |
| Observer agreement | observer lens sees marker and terminal; awaiter resolves | | pending and reconciled checkpoints |
| Docs agreement | every scored claim matched observed evidence | | `04-docs-claims.txt`, `04-request-abandon.png` |
| Teardown | panic gate clean; owned container and docs port gone | | `request-abandon-e2e.log`, container inventory |

**Aggregate:** would an operator following only the published escape hatch preserve a live
owner's authority, wait out ambiguity, and obtain exactly one truthful abandonment fact
that every observer can see?

---

_Stop triggers and the Abort/RCA protocol are in [../RULES.md](../RULES.md). A docs versus
behavior divergence is a product finding: preserve artifacts and stop._
