# Real-host E2E harness, selection and receipts

The real-host E2E harness boots durable lash nodes as real host processes,
drives them only through their product surfaces, faults them, and reads back
what they committed. There is no server beside the nodes (ADR 0132 §1) and no
test-only engine hook: every node is a lash node built through the public
facade over the case's store.

## The catalogue

`scripts/lash-e2e-manifest.json` owns scenario selection. It lists S01–S37
except S19 and S20, which exercised the OS-worker process engine lash no
longer ships (FIG-5158). For each scenario it lists the named laws and
risks, owner lanes, store/leg/channel/variant permutations, required
artifacts and landing dependencies. Every permutation row is in one state:

- `ready`: the row names its registration, the runner label
  `//crates/lash-e2e:e2e__test` and the full test path `module::test`, and
  runs through `scripts/e2e-gate.py`.
- `held`: the row keeps a named `hold_reason` and no registration. Plans list
  it; `run` and `reconcile` refuse it.
- `retired`: the row's subject left lash. It keeps a written `disposition`
  that names the removing commit and the law that proves whatever guarantee
  survives. A retired row counts in the tier catalogue checks, and plans list
  it under `retired`. It is never selected, run or certified.

Plans are inventory, never passing execution evidence. Registration states
are distinct from `arc_guards`. Arc guards name the exact product ticket and
its landing commit; they refuse certification until that commit is in
candidate and main ancestry. S22 guards FIG-4896–4900 and S18's final wait
contract guards FIG-4897; both are landed.

Smoke selects exactly S01/S02/S17/S18/S26/S30 on the live leg. S01/S02/S17/S18/S26
use file SQLite; S30 uses memory SQLite. Full and release select the same
deterministic catalogue; the paid live-provider cases S35/S36 are a separate
`live` tier.

On 2026-10-08 the release catalogue holds 132 rows:
- 92 are `ready`.
- 20 are `retired`: S10, S13, S24, S33 and S09's before-intent variant. Their
  subjects went with the Run journal (FIG-5174) or with build generations and
  finalize (FIG-5200).
- 20 are `held`: L13 (FIG-5193) re-scopes S22, S23, S31 and S32, so a release
  run refuses until it lands.

## The harness

`crates/lash-e2e` is the harness. Its library owns the case, its nodes, the
control endpoint and the receipt. Its one integration test target,
`//crates/lash-e2e:e2e__test`, holds every case, one Rust module per
scenario family:

| Module | Scenarios |
| --- | --- |
| `workbench` | S01, and the helpers every workbench case shares |
| `tools` | S02, S03, S05, S30 |
| `state` | S04, S25 |
| `retries` | S06, S07 |
| `intents` | S08, S09 |
| `handover` | S11, S12 |
| `fleet` | S14, S15, S16 |
| `operations` | S17 |
| `cancel` | S18 |
| `waits` | S21 |
| `provider` | S26, S27 |
| `mcp` | S28 |
| `browser` | S29 |
| `telemetry` | S34 |
| `feeds` | S37 |

Every case is an ignored test that refuses without its runner's
environment, so a plain `kiln test` of the target passes nothing off as
proof. A case runs as `Case::run(name, store, leg, budget, body)`, and that
call writes the case receipt whether or not the body passed.

### Hosts

`Case::boot(host, node, options)` starts one lash node. It checks the binary
against the digest the runner built and records it as a case artifact.

- `Host::Consumer` is `examples/e2e-consumer`, a host built from the public
  facade alone. Its fixture scripts the provider's steps and the tool bodies.
- `Host::Workbench` is `examples/agent-workbench` with its `e2e-tools`
  feature. The feature adds host-side fixtures and routes only:
  - the H2 scripted provider and tool bodies;
  - the receiver, follow, drain and cut-release routes under `/api/e2e`.

  The engine and its stores are the product's.

A node boots under its own name, which is its lease owner identity in the
store's fleet. Two boots of one name are one node restarted; another name is
another node.

Each node writes a commit ledger: every durable commit labelled with its
actor and epoch, every reap, and every commit held at one of the case's cuts.
The ledger is how a case holds a node at an exact commit (`Case::held`), and
how the receipt counts hidden replay (ADR 0132 §2).

A case faults nodes with:
- `kill` (SIGKILL);
- `stop` (SIGTERM, with the clean shutdown as a cleanup receipt);
- `freeze`/`thaw` (SIGSTOP/SIGCONT);
- a `Proxy` between a node and PostgreSQL that it can `partition` and
  `heal`.

### The control endpoint

Every case runs one HTTP control endpoint that the hosts reach:

- **Tool bodies:** holds and releases bodies, and records the deliveries an
  effect recipient accepted, so a case can count executions per identity.
  The H2 bodies post their deliveries to it too.
- **Recorded provider:** an OpenAI-compatible provider that the workbench's
  production client calls. A case scripts each request's reply: streamed
  text, tool calls and usage, a refusal status, a hold before a delta, or a
  connection reset. It also keeps every request.
- **OTLP collector:** keeps the spans a host exports, and can refuse exports
  as a disconnected collector would.

### Peers and browsers

`Case::peer` runs a supporting process beside the nodes and kills it as a
fault. S28 uses this for the workbench's own MCP fixture server
(`agent-workbench mcp-fixture http`). A peer is no lash node.

S29 reads the product page through headless Chromium. Its case runs
`crates/lash-e2e/tests/e2e/timeline.py` with the runner's Playwright
interpreter (`LASH_E2E_PYTHON`, Playwright 1.62.0, browsers under
`PLAYWRIGHT_BROWSERS_PATH`). It then compares every drawn transcript row with
the committed API transcript and the store.

## Stores, legs and live replay

Every case boots its own lash nodes over the row's store:

- one SQLite file, or one SQLite memory store, for a single node at a time;
- one PostgreSQL store that several nodes share.

A row's `nodes` is how many distinct lash nodes the case boots. For a
PostgreSQL store or live replay store, the runner supplies PostgreSQL
through `scripts/ci/with-service.sh pg`. The case then creates a fresh
database for itself (`LASH_POSTGRES_DATABASE_URL`).

Each permutation is its own test function. Its name joins the scenario's
test name, its variant, and `_postgresql` and `_resume` where the store and
leg are not a single-node scenario's default. A PostgreSQL live replay
variant's name ends `_live_replay`.

A row may declare `"live_replay": "postgresql"` (variant
`postgresql-live-replay`):
- the planner passes `--live-replay postgresql`;
- the runner sets `LASH_E2E_LIVE_REPLAY`;
- every workbench of the case runs on the shared PostgreSQL live replay store
  (FIG-5101), while its durable store stays the row's `store`.

A case whose name ends `_live_replay` refuses any other selection.

A `resume` leg kills a node at the scenario's cut, after the work it names
has committed, and resumes that work on another node from committed state
alone. Its `nodes` is at least two; on SQLite the second node opens the file
after the first is dead. The leg oracle needs a killed node and a node that
resumed its work in the case's node evidence. Every leg reports the labelled
commits it observed with the replay tripwire's counts, so a body run twice,
or an outcome looked up for re-running code, is visible in the `commits`
artifact.

## Running and certifying

Read a plan without booting anything:

```console
. ./env.sh
python3 scripts/lash-e2e.py plan --tier smoke \
  --sha "$(git rev-parse HEAD)" --artifacts target/e2e-plan
```

These selectors refuse:
- an unknown scenario;
- a selector absent from its tier;
- duplicate selectors;
- a selection that is empty or entirely retired.

`--scenario Snn` and `--case <key>` narrow non-release tiers. A release plan
always selects the whole release catalogue. `--ready` drops held rows from a
non-release plan and marks it `tier_complete: false`.

`run` executes each selected case through the committed runner. It requires
a clean checkout at the exact SHA and a fresh artifact directory:

```console
python3 scripts/lash-e2e.py run --tier full --ready \
  --sha "$(git rev-parse HEAD)" --artifacts target/e2e-full
```

For each case the planner calls `python3 scripts/e2e-gate.py <label> <test>
--store <store> --leg <leg> --live-replay <store> --artifacts
<dir>/case-<i>`. That one call runs a single case on its own, too. The
runner then does four things:
1. enters the fork's private Kiln gate;
2. builds the hosts, the VM worker and the harness in one `kiln build`;
3. prepares its Playwright interpreter;
4. runs an exact uncached `kiln test` of the registered test.

Its JUnit must contain exactly the registered test. The runner then splits
the case receipt into the role artifacts that reconcile reads.

The paid live rows (S35's three RLM workspace cases, S36's workbench weather)
take their provider from the operator's environment, which the runner
forwards:
- `OPENROUTER_API_KEY` and `OPENROUTER_MODEL`;
- `LASH_E2E_OUTPUT_TOKEN_CAP`;
- `LASH_E2E_LIVE_BUDGET`, a capped account policy: `model`, `max_calls`,
  `max_input_bytes`, `max_output_tokens`, `max_spend_usd`, the per-token
  rates `input_usd_per_token` and `output_usd_per_token`, and a relative
  `receipts` path.

Missing credentials produce `not_run`, which cannot certify a deterministic
tier.

## Receipts

The producer writes `receipt.json` using
`scripts/lash-e2e-receipt.schema.json`. The receipt carries:
- the source SHA, manifest digest and tier;
- exactly one receipt per selected case;
- aggregate counts, and counts per store/leg.

The case key joins scenario, variant, store, leg and channel in that order.
Status is `passed`, `failed` or `not_run`, and `executed` agrees with it.
Quarantine cannot certify.

Each case supplies digest-qualified relative paths for its commits, store,
host, trace, cleanup, JUnit and provenance artifacts:
- Files must exist beneath the receipt directory, and match their digests.
- JUnit must contain exactly the registered test, with no failure, error or
  skip.
- Cleanup must be `{"complete":true,"errors":[],"remaining":[]}` once every
  node, peer, browser and the control endpoint has closed.
- Provenance carries the case and source SHA, the number of lash nodes the
  case booted (which must equal the row's `nodes`), and every manifest binary
  role with its exact source SHA and artifact descriptor.

Reconcile preserved artifacts independently:

```console
python3 scripts/lash-e2e.py reconcile --tier release \
  --sha <exact-candidate-sha> --receipt target/e2e-release/receipt.json \
  --artifacts target/e2e-release
```

A successful conclusion requires:
- selected = executed = passed > 0 in every store/leg group;
- zero failed or `not_run` cases, and no quarantine;
- every artifact, and complete cleanup.

Release additionally requires:
- the F04/Z0A/Z0P/Z01/Z02/Z03/Z04/Z05 audit commits in both candidate and
  `origin/main` ancestry, each with a retained ticket gate receipt that names
  its ticket, audit SHA and `passed` status;
- Phase A, facade and schema gate receipts that name the exact candidate SHA
  and a passed status.

Another SHA's success never certifies the candidate.

Artifact directories belong to one immutable plan:
- Preserve first-failure artifacts. A diagnostic rerun uses a separate
  directory and cannot replace that failure.
- Conclusions carry receipt and manifest digests; consumers must compare
  these with the retained inputs.
- Event barriers and deadline-bound probes establish readiness, completion
  and cleanup. Sleeps never do.

The laws in `scripts/test_lash_e2e.py` pin R8's selection rules:
- counts and artifacts;
- the resume leg;
- retired rows;
- release evidence.

They use synthetic envelopes, execute no host case, and give no
live-substrate or release proof.
