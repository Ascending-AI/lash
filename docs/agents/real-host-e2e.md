# Real-host E2E selection and receipts

`scripts/lash-e2e-manifest.json` owns scenario selection. It lists S01–S36,
their named laws/risks, owner lanes, store/leg/channel/variant permutations,
required artifacts and landing dependencies. Every implemented case is
registered and runs through `scripts/e2e-gate.py`; held rows keep a named
reason and refuse execution and certification. Plans are inventory, never
passing execution evidence. Runtime registration holds are distinct from
`arc_guards`. Arc guards name the exact product ticket and its landing commit;
they refuse certification until that commit is in candidate and main ancestry.
S22 guards FIG-4896–4900, S13's final drain path guards FIG-4900, and S18's
final wait contract guards FIG-4897; all are landed. F02/FIG-1863 was already
landed at intake; S35/S36 have no F02 guard.

Smoke selects exactly S01/S02/S17/S18/S26/S30 on the live leg. S01/S02/S18/S26
use file SQLite; S17/S30 use memory SQLite. Full and release select the same
deterministic catalogue; live-provider cases S35/S36 are separate. Counts cover
each permutation. Held rows keep their variant hold until the named owner
lands its implementation. S33 reuses the existing Phase A operator
choreography; it does not introduce another operator supervisor. Existing
supervisors and the Restate law board remain until parity.

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

Every ready registration names an ancestor commit, one of the runner's labels
and a full test path. Execution requires a clean checkout at the exact SHA.
For each case the planner calls
`python3 scripts/e2e-gate.py <label> <test> --artifacts <dir>/case-<i>`. The
runner enters the fork's private Kiln gate, materializes the union of
binaries the registered selectors need in one `kiln build`, serves pinned
Restate, and runs an exact uncached `kiln test` under
`scripts/ci/restate_suite.py`. Its JUnit must contain exactly the registered
test. Stale case or test selectors refuse before boot. This script owns no
service processes and inserts no journal commands.

The producer writes `receipt.json` using
`scripts/lash-e2e-receipt.schema.json`. It carries the source SHA, manifest
digest, tier, exactly one receipt per selected case, aggregate counts and counts
per store/leg. The case key joins scenario, variant, store, leg and channel in
that order. Status is `passed`, `failed` or `not_run`; `executed` agrees with it.
Quarantine cannot certify. The schema describes the envelope; the reconciler
also enforces artifact contents and cross-record invariants.

Each case supplies digest-qualified relative paths for journal, store, host,
trace, cleanup, JUnit and provenance artifacts. Files must exist beneath the
receipt directory, including through symlinks, and match their digests. Scenario
owners retain decoded journal/barrier/fault and business evidence in these
artifacts; a provider log cannot substitute for a journal. JUnit must contain
exactly the registered test, with no failure/error/skip. Cleanup must be
`{"complete":true,"errors":[],"remaining":[]}` after all owned resources close.
Provenance carries case/source SHA, negotiated `V7`, server-node count, all
manifest binary roles with exact source SHAs and artifact descriptors, and
the server version/archive digest/executable descriptor. The initial server
archive pin is the existing native-pool pin, 1.7.13; its executable digest
remains unregistered until H0 supplies it. Activation must use one pin across
both legs; this change does not alter the independent live-law runner's pin.

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

The seven laws in `scripts/test_lash_e2e.py` pin R8 selection, counts, artifact and
release-evidence rules using synthetic envelopes. They execute no host cases
and provide no live-substrate, upgrade or release proof.
