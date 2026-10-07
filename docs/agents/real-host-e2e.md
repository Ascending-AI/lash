# Real-host E2E selection and receipts

`scripts/lash-e2e-manifest.json` owns scenario selection. It lists S01–S37
except S19 and S20, which exercised the OS-worker process engine lash no
longer ships (FIG-5158): lash's cancellation is cooperative, and hard
isolation is the host engine's own. For each scenario it lists the named
laws/risks, owner lanes, store/leg/channel/variant permutations,
required artifacts and landing dependencies. Every implemented case is
registered and runs through `scripts/e2e-gate.py`; held rows keep a named
reason and refuse execution and certification. Plans are inventory, never
passing execution evidence. Runtime registration holds are distinct from
`arc_guards`. Arc guards name the exact product ticket and its landing commit;
they refuse certification until that commit is in candidate and main ancestry.
S22 guards FIG-4896–4900, S13's final drain path guards FIG-4900, and S18's
final wait contract guards FIG-4897; all are landed. F02/FIG-1863 was already
landed at intake; S35/S36 have no F02 guard.

Every case boots its own lash nodes over the row's store, with no server
beside them (ADR 0132 §1): one SQLite file or SQLite memory store for a
single node, or one PostgreSQL store that several nodes share. A row's
`nodes` is how many lash nodes the case boots.

Smoke selects exactly S01/S02/S17/S18/S26/S30 on the live leg. S01/S02/S17/S18/S26
use file SQLite; S30 uses memory SQLite. Full and release select the same
deterministic catalogue; live-provider cases S35/S36 are separate. Counts cover
each permutation. Held rows keep their variant hold until the named owner
lands its implementation. S14 kills one of two nodes over one PostgreSQL
store mid-tool and S15 partitions one from the database; the survivor reaps
it, claims its actors and finishes the work, and the fenced node's late
commits are refused. S37 boots two workbench nodes over one PostgreSQL
store, runs a turn on node B while node A's feeds observe it, kills B
mid-turn and resumes the turn on A. Its variant names the live replay store
both nodes run with: with the process-local `memory` store, B's live
activity never reaches A and A converges through the durable head; with the
shared `postgresql` store (FIG-5101), A's feeds carry B's live activity
before commit and converge without a gap. S33 reuses the existing Phase A
operator choreography; it does not introduce another operator supervisor.

2026-10-07: every row is held. The E2E host harness went with the server
it booted (FIG-5190), and no host can run a turn through the facade until
L3's facade wiring (FIG-5172) lands; L9h (FIG-5186) then
rebuilds the harness and registers the rows. Rows whose subject was removed
machinery (S12, S13, S22, S23, S24, S31, S32) name the lane that re-scopes
or retires them.

Read a plan without booting services:

```sh
. ./env.sh
python3 scripts/lash-e2e.py plan --tier smoke \
  --sha "$(git rev-parse HEAD)" --artifacts target/e2e-plan
```

An unknown scenario, a selector absent from its tier, duplicate selectors, and
an empty selection refuse. `--scenario Snn` can narrow non-release tiers. A
release plan always selects the whole release catalogue. Held rows appear in
`plan.json`; `run` and `reconcile` refuse them. Missing credentials in the
optional live tier produce `not_run`, which cannot certify a deterministic tier.

`run` executes each selected case through the committed runner:

```sh
python3 scripts/lash-e2e.py run --tier smoke \
  --sha "$(git rev-parse HEAD)" --artifacts target/e2e-smoke
```

The paid live rows (S35's three RLM workspace cases, S36's workbench weather)
take their provider from the operator's environment, which the runner
forwards: `OPENROUTER_API_KEY`, `OPENROUTER_MODEL`, `LASH_E2E_OUTPUT_TOKEN_CAP`
and `LASH_E2E_LIVE_BUDGET`, a capped account policy (`model`, `max_calls`,
`max_input_bytes`, `max_output_tokens`, `max_spend_usd`, per-token
`input_usd_per_token`/`output_usd_per_token` and a relative `receipts` path).
Each case writes its own copy of the policy and its usage receipts inside its
case directory, so one policy serves every selected case. S35 builds the
`//runbooks/rlm-smoke:rlm-smoke` host and needs Docker for its jailed exec;
S36's collection still needs the runbook's judgement before it certifies.

Every ready registration names one of the runner's labels and a full test
path that must exist in this tree; there is no registration commit, and the
runner's exact-one-JUnit check refuses a stale or zero selection. Execution
requires a clean checkout at the exact SHA.
For each case the planner calls
`python3 scripts/e2e-gate.py <label> <test> --store <store> --leg <leg>
--artifacts <dir>/case-<i>`. The
runner enters the fork's private Kiln gate, materializes the union of
binaries the registered selectors need in one `kiln build`, and runs an
exact uncached `kiln test`. Its JUnit must contain exactly the registered
test. Stale case or test selectors refuse before boot. The case boots and
stops its own nodes; this script owns no service process but PostgreSQL.

Each row declares its store and leg, and every permutation is its own test
function named `<test>[_postgresql][_resume]`. A row may also declare
`"live_replay": "postgresql"` (variant `postgresql-live-replay`): the planner
passes `--live-replay postgresql`, the runner supplies PostgreSQL as for a
PostgreSQL store and sets `LASH_E2E_LIVE_REPLAY`, and every workbench of the
case whose test names no live replay store runs on the shared PostgreSQL live
replay store, in a schema of the case's namespace (FIG-5101). Its durable
store stays the row's `store`. S01/S02/S17/S18/S26 carry such a row in full
and release; S30's host is the external consumer, which has no live replay
seam. For `--store postgresql` the
runner supplies PostgreSQL through `scripts/ci/with-service.sh pg`, and the
case creates a fresh database and applies the committed schema itself.

A `resume` leg replaces the old forced-replay leg and keeps its crash
coverage: the case kills a node at the scenario's cut, after the work it
names has committed, and resumes that work on another node from committed
state alone. Its `nodes` is at least two (on SQLite the second node opens
the file after the first is dead). The leg oracle needs a killed node and
a node that resumed its work in the case's node evidence; every leg reports
the labelled commits it observed with the replay tripwire's counts (ADR 0132
§2), so a body run twice or an outcome looked up for re-running code is
visible in the `commits` artifact.

The producer writes `receipt.json` using
`scripts/lash-e2e-receipt.schema.json`. It carries the source SHA, manifest
digest, tier, exactly one receipt per selected case, aggregate counts and counts
per store/leg. The case key joins scenario, variant, store, leg and channel in
that order. Status is `passed`, `failed` or `not_run`; `executed` agrees with it.
Quarantine cannot certify. The schema describes the envelope; the reconciler
also enforces artifact contents and cross-record invariants.

Each case supplies digest-qualified relative paths for commits, store, host,
trace, cleanup, JUnit and provenance artifacts. Files must exist beneath the
receipt directory, including through symlinks, and match their digests. Scenario
owners retain labelled commits, tripwire counts, barrier/fault and business
evidence in these artifacts; a provider log cannot substitute for the
commits. JUnit must contain
exactly the registered test, with no failure/error/skip. Cleanup must be
`{"complete":true,"errors":[],"remaining":[]}` after all owned resources close.
Provenance carries case/source SHA, the number of lash nodes the case
booted, which must equal the row's `nodes`, and all manifest binary roles
with exact source SHAs and artifact descriptors.

Reconcile preserved artifacts independently:

```sh
python3 scripts/lash-e2e.py reconcile --tier release \
  --sha <exact-candidate-sha> --receipt target/e2e-release/receipt.json \
  --artifacts target/e2e-release
```

A successful conclusion requires selected = executed = passed > 0 in every
store/leg group, zero failed/not_run, no quarantine, all artifacts and complete
cleanup. Release additionally requires F04/Z0A/Z0P/Z01/Z02/Z03/Z04/Z05 audit
commits in both candidate and `origin/main` ancestry. Each audit's retained
ticket gate receipt names its ticket, audit SHA and `passed` status. Phase A,
facade and schema gate receipts must name the exact candidate SHA and passed
status. The activation workflow must fetch sufficient Git ancestry to verify
these claims. Another SHA's full-profile success never certifies the candidate.

Artifact directories belong to one immutable plan. Preserve first-failure
artifacts; `run` requires a fresh directory, and a diagnostic rerun uses a
separate directory and cannot replace that failure. Conclusions carry receipt
and manifest digests; consumers must compare these with the retained inputs.
Event barriers and deadline-bound predicate probes belong in the
case and its evidence; sleeps cannot establish readiness, completion or
cleanup.

The laws in `scripts/test_lash_e2e.py` pin R8 selection, counts, artifact,
resume-leg and release-evidence rules using synthetic envelopes. They execute no host cases
and provide no live-substrate, upgrade or release proof.
