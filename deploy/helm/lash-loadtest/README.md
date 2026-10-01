# Lash load-test topology

This chart owns the synthetic FIG-3790 topology. It is separate from product
hosting because it deploys a scripted provider, independent witness database,
smoke driver and collector with its own resource caps. It reuses the current
Restate/PostgreSQL runbook's worker and provider binaries. Restate uses the
existing official 1.7.12 image pinned by digest, without adding an operator or
changing runtime formats.

Run the local proof in a build-enabled Kiln fork:

```sh
. ./env.sh
LASH_LOADTEST_FAULT_CAMPAIGN=false kiln gate lash fig-4171 -- just multi-node-load local
```

`multi-node-load` is the named on-demand recipe: it installs the chart, runs
the workload, collects and archives the results, then uninstalls. It takes a
target, `local` or `scaleway`. `local` runs on kind; `scaleway` is PENDING
until the Kapsule cluster, registry and operator credentials exist and exits
with an explicit refusal today. The recipe is run on demand only and is never
dispatched per PR.

The foreground recipe builds the harness binaries on the shared pool, plus the
synthetic N+1 worker and operator binary. It creates both generations'
runtime images, boots a three-node kind cluster, and installs this chart. It
installs pinned Helm and kind binaries under the fork's `target/` when missing.
Docker, kubectl, Python with PyYAML, curl and openssl must be available. The
runtime image builds from repository source and the downloaded pool outputs.
The recipe loads the repository image for the host platform and clears host
proxy variables when starting kind. The nodes pull pinned service images through
their direct registry connection.

The proof disables automatic Restate provisioning, provisions once, adds the
three metadata members, and inspects committed node, partition and log metadata.
It requires replication factor two and distributed log sequencing. It then
holds one node down, waits for all 24 leaders, followers and sequencers to
stabilize on the two surviving members, then runs a synthetic public session send through the
proxy and the Restate engine. Both worker processes reopen the shared session,
observe committed PostgreSQL attachment ownership, and read identical attachment
bytes through the public S3 storage contract. The provider and client write the
separate witness database. Snapshot creation with log trimming must succeed
against the pinned server. The restarted node must rejoin with its original ID
and an advanced incarnation; all three metadata members must be alive before
all three Restate Prometheus targets are accepted as recovered.

Each Restate node, PostgreSQL and Garage has its own persistent volume claim.
Attachments and Restate snapshots use separate object prefixes. When enabled,
network shaping applies delay, jitter and bandwidth once at each worker,
Restate, PostgreSQL and Garage pod's outbound interface. The synthetic provider,
proxy, driver and collector have separate resource budgets. The local values
reduce resource requests and limits for development; they do not establish a
release performance baseline.

The recipe uses an explicit run kubeconfig for every Kubernetes command. Names
come from `KILN_GATE_ID`. Logs, rendered manifests and cluster evidence remain in
`target/fig-3790/<gate>-<timestamp>/`. Cleanup removes only that namespace and
kind cluster, including their volumes, and fails if they remain. It never
changes the user's Kubernetes context. Credentials are generated for each local
run and installed as a Kubernetes Secret. The chart contains no credentials and
does not create a Secret.

# Results archive and manifest

After the workload drains, the recipe reconciles the measurements and archives
the run under `results/fig-3790/<run-id>/` inside the run's evidence
directory. The archive contains the versioned measurement files
(`operations.jsonl`, `metrics.jsonl`, `samples.jsonl`, `faults.jsonl`,
`histograms.json`, `sample_errors.jsonl`, `collection_gaps.jsonl`, `query_retries.jsonl`,
`witness_evidence.jsonl`, `witness.json`, `collection.json` and
`summary.json`), an `evidence/` directory with the topology, build and
placement inputs (merged run values, rendered topology, kind config, image
digests, source and hardware provenance, pod placement, Restate membership,
replication, partition placement and raw fault rows), and a `manifest.json`.
Every result file declares `schema_version: 1`; `SHA256SUMS` covers every
archived file, and `<run-id>.tar.gz` beside the directory (with
`<run-id>.tar.gz.sha256`) is the transport unit. The manifest builder validates
the run identity across files, the workload hash against the run record, the
required evidence set and every declared schema version, and it runs for
failed or incomplete analyses so retained evidence stays self-describing.

`manifest.json` records the run's source provenance (the lash SHA and dirty
flag the images were built from, plus the workload's inventory lash and
figments SHAs), the workload name/hash/format, the seed and generator, the
built image digests and pinned Restate/kind images, host hardware, cluster and
pod placement, the shaped link settings, the installed settings hash,
warmup/prefill/session parameters, the reconciliation qualification and the
baseline ID — null today, because no baseline exists. Set
`LASH_LOADTEST_RESULTS` to a durable path to write the archive somewhere other
than the run directory. A smoke archive establishes no baseline, saturation
estimate or budgets; compare only runs with matching target, profile, hardware
and settings.

Override local resources through a values file with `LASH_LOADTEST_VALUES`.
The v1 proof requires three Restate nodes, 24 partitions, replication two and run-owned Garage.

Check the chart without starting services:

```sh
kiln gate lash fig-4167 -- just loadtest-chart-check
```

The repository script gates run the same Helm lint, template and evidence
checker tests. They render local, Scaleway, external S3 and retained-generation
profiles. The live topology is on demand and is not dispatched by these gates.

# Values and fault-controller handoff

`values.yaml` sets the topology, resources, pinned infrastructure images,
storage sizes/classes, shaping and scrape interval. `values-local.yaml` is the
local proof profile. The schema rejects malformed topology settings before
installation. `driver.enabled` controls the bounded smoke Job; it is disabled
until the local controller has provisioned Restate. Schema setup is an install
hook and is not reapplied during compatible upgrades. The bounded driver is an
install/upgrade hook so Kubernetes never has to mutate a completed Job. Disable
it for L4 campaigns that supply their own sustained driver. The driver is the L2 smoke.

# Durable workload

`load.enabled` renders the `<name>-load` Job, which runs `lash-loadtest-driver`
against the running topology (FIG-4168). It is off at install; `just
multi-node-load` applies it after the topology gates under the fault campaign
below, waits for it, archives its log as `load.log` and `load-witness.txt`,
and fails unless the controller completed its campaign and the driver printed
`load witness verdict=passed`. `load.workload` names the checked-in
workload every worker, the provider and the driver generate from (`smoke-v1`
or `figments-v1`); `load.sessions` and `load.turnsPerSession` bound the run.
The default is the pipeline smoke: four sessions of at least six turns, running
until the fault campaign ends, a few minutes. It is not a baseline and derives
no budgets.

Every session is an open-loop Poisson clock of primary turns sent through
Restate ingress to the workers' `E2eLoadWorkflow`, which runs each through
lash's public API: `session.send` with its queued inputs and cancels, RLM cells
calling synthetic tools, child processes with awaits and delayed signals,
host process starts with signals, cancels and awaits, attachment puts, cron
schedules registered as trigger subscriptions and emitted on their seeded
phases, and deletes of rotated or deleted sessions. Workers read every blob
back through their `/load/attachments` endpoint, rotating so a peer reads what
another worker put. Deleting the cron owner ends its subscriptions; one more
emission per schedule must start nothing.

Before the open-loop actors start, a bounded workflow drives FIG-4241's
additional operations. It appends the generated history and reopens the session,
opens an administrative summary frame, then reaches the context-pressure
threshold and checks that the next turn runs in a separate summary frame. It
runs an auxiliary `llm.query`, registers a non-cron external subscription,
emits and awaits its target, updates and reads its revision, and deletes it
before a final emission. Promotion discovery reads the session-originated
process record and resolves its module artifact and process reference. Each
operation has its own witness class and checks. The smoke uses two prefill
turns; it performs no baseline measurement.

The provider sends the planned streamed replies as timed SSE chunks. A root
that admits several inputs gets one cell containing every admitted turn's tool,
attachment and child-process plan. Its finish value lists those input keys,
and the provider receipts each input. The witness checks every turn's tool
coverage, including when it shares a root with an earlier queued input.

The evidence lives in the witness database, outside lash's store: the driver's
sent and terminal rows, the provider's receipts, the synthetic tools' effect
attempts and commits, and the exact blob bytes put and read (digested by the
witness). The driver then reconciles 27 workload evidence classes against the plan the
workload regenerates and prints one `load witness class=...` line per class.
Any violation, and any class with no evidence, fails the run. The measurements
collector archives the independent ledgers and reconciles them against the
client operation records before qualifying the result.

# Fault campaign

`just multi-node-load` runs the durable workload under the fault controller
(FIG-4169, `scripts/loadtest_faults.py`). The load Job gets
`load.faultCampaign: true` and a shared `load.run`, so its sessions keep their
open-loop clocks running until the controller ends its campaign. The driver
never takes a lost answer as a terminal: it resubmits under the same workflow
key and attaches to the invocation Restate already accepted, and re-reads a
blob from a worker the fault took down once it serves again.

Fault times, restart delays, the warm-up and the recovery hold come from the
workload's `faults`, `warmup_s` and `collection.recovery_stable_s`. The fault
phase starts after the warm-up that follows the driver's first send:

1. **Worker kill.** The controller reads every worker's running load
   operations (`GET /load/active` on its control port), leaves a restart hold
   in the busiest worker's `/fault` volume and SIGKILLs its process. The pod
   shares its process namespace, so the worker is not PID 1. The kubelet
   restarts the container in the same pod, which waits out the hold once
   before serving at the identical endpoint. The restart is the pod's
   restart count and the hold marker. The exit code is judged when the
   kubelet still reports the stopped container, and recorded as null when
   it does not.
2. **Restate node restart.** From `restatectl sql` it picks the node leading
   the most partitions whose applied log advanced between two samples, then
   restarts `restate-server` in place with SIGTERM after the same hold. The
   node must rejoin with its original ID and a new generation, every
   partition must have a leader, and every partition it led must be
   re-elected.
3. **Rolling deploy.** A Helm upgrade starts the `faults.rollingGeneration`
   workers from the synthetic N+1 image beside the running generation. Their
   pre-upgrade `lashctl migrate` hook expands the store with the replacement's
   own operator binary first. The replacement registers its immutable proxy
   URL through lash's registration, which moves new admission. After
   `rolling_worker_pause_s`, the controller calls `drain_generation(old)` from
   the replacing build (`POST /generations/{G}/drain`). It retires the old
   Deployments only once `generation_drain_status(old)` is drained, no
   stalled obligation remains, and no unfinished Restate invocation is pinned
   to the old deployment.

Before each fault the controller waits for busy work, and it fails the
campaign if none appears. After each fault it waits for recovery:

- every operation in flight at the injection answered;
- after the fault, a turn, a queued input and a cron emission all answered;
- the backlog returned to its pre-fault range, the maximum sampled in
  healthy windows;
- the target recovered.

A 300 s drain watchdog (the workload's `drain_timeout_s`) bounds each wait;
it is a test timeout, not a budget. The controller then holds the stable
window and records completed throughput and latency around the fault,
without gating them until the baseline sets budgets.

Every step is a `witness_load_faults` row on the witness clock: intent,
injection with the busy work it hit, recovery evidence, failure and campaign
end. The driver's verifier adds the `fault-campaign`, `worker-kill`,
`restate-restart` and `rolling-deploy` classes, derived from the witness:

- each fault hit operations in flight;
- every one of them reached a durable answer;
- service progressed afterwards;
- turns after the rolling deploy were answered by the replacement's workers;
- the controller's recovery evidence holds.

`faults.jsonl`, `faults.log` and the kubectl transcript are archived with the
run. The `<name>-faults` ConfigMap's `targets.json` names the controller's
choices: the Restate pods, the worker count, both generations, the partition
count and the fault probe Deployment. The probe is the controller's in-cluster
hand for the witness, Restate's admin API and the worker control endpoints.

For a replacement generation, set `workers.generation`, retain the old name in
`workers.retainedGenerations`, and record both immutable tags in
`workers.generationImages`. Keep `image.tag` at the original bootstrap image.
Every generation gets independent worker Deployments and a proxy Service
named `<name>-workers-<generation>`. A generation with its own image in
`workers.generationImages` also renders its migrate hook. Worker `i`'s
control port stays reachable across generations as `<name>-worker-<i>-control`.

# Rolling-upgrade campaign

`just e2e-rolling-cluster` runs this topology with
`scripts/loadtest_upgrade.py` in place of the fault controller: N and the
synthetic N+1 side by side under the same load, through the ADR 0106 §6
choreography. Worker generations `initial` (N), `next` (N+1), `rollback`
(N) and `final` (N+1) each serve their own URI; each image carries its
own build's `lashctl`, run inside that generation's pod. The rollback
generation sets `workers.migrate: false`, so N never runs its migrate over
N+1's expansion. The campaign skips the quorum-loss hold and the results
archive, and the driver judges the upgrade classes of
`runbooks/restate-postgres-workers/src/load/upgrade_verify.rs`. The
runbook is `runbooks/rolling-upgrade/`.

# Scaleway profile

Scaleway provisioning and release qualification are PENDING until the account
and credentials are supplied. No cloud resources are provisioned by this lane.
`values-scaleway.yaml` selects `sbs-5k` and separate node pools through the
operator-created `lash-loadtest-pool` labels. Provide at least three Restate
nodes because this profile requires pod anti-affinity. Size the pools to satisfy
the resource requests in the base values and record actual instance placement
when establishing the baseline.

Create the namespace and the Secret named by `credentialsSecret` before
installing. Its keys are `postgres-password`, `witness-password`, `database-url`,
`witness-database-url`, `s3-access-key`, `s3-secret-key` and `garage-rpc-secret`.
Both database URLs address the chart's PostgreSQL Service and their respective
databases. The image repository and immutable tags must point at a registry
reachable from Kapsule. Supply values in an operator-owned override file:

```sh
helm install topology deploy/helm/lash-loadtest --namespace "$namespace" \
  -f deploy/helm/lash-loadtest/values-scaleway.yaml -f "$operator_values" \
  --set driver.enabled=false
```

After the schema Job and three Restate pods are ready, provision the cluster
once with `restatectl provision --replication 2 --num-partitions 24 --yes` in
Restate pod zero. Use the replication and partition counts from the installed
values if overridden. Verify all three metadata members and committed log and
partition placement, then enable the driver with a Helm upgrade using the same
values. The local recipe demonstrates this sequence and archives its evidence.

The default Scaleway profile uses in-cluster Garage. For Scaleway Object
Storage, override `s3.mode: external`, `s3.externalEndpoint`, `s3.region` and
`s3.bucket`; create the bucket first and supply its credentials through the
same Secret. Restate uses HTTPS for this profile. Do not store keys in values.

The server settings and metadata checks follow the
[Restate 1.7.12 source](https://github.com/restatedev/restate/tree/v1.7.12), including
its explicit metadata-member operations and snapshot/trim command. The local
node image follows the [kind 0.29.0 release](https://github.com/kubernetes-sigs/kind/releases/tag/v0.29.0).

The Scaleway profile uses the documented `sbs-5k` class from
[Scaleway CSI storage guidance](https://www.scaleway.com/en/docs/kubernetes/api-cli/managing-storage/).
Confirm that class and the operator-created pool labels on the actual cluster
before provisioning the pending release baseline.

# Measurements

The load driver collects measurements before its first admission, periodically
through the load, and after the drain. `just multi-node-load` reconciles its
records and writes `results/fig-3790/<run-id>/` inside the run's evidence directory. It produces
version 1 `operations.jsonl`, `metrics.jsonl`, `samples.jsonl`, `clock_anchors.jsonl`, `recovery_inputs.jsonl`, raw `faults.jsonl`,
`histograms.json`, `sample_errors.jsonl`, `collection_gaps.jsonl`, `query_retries.jsonl`,
`witness_evidence.jsonl`, `witness.json`, `collection.json` and `summary.json`,
then archives them with topology, build and placement evidence under
`evidence/` plus the versioned `manifest.json` described above. A smoke
result has no baseline, saturation estimate or budgets.

Each operation preserves the driver's monotonic scheduled, send, acceptance
and terminal-observation times, its actor, session, subject, input and invocation IDs, request/response
sizes, typed outcome, response and terminal journal read. Restate's `/send`
acknowledgement establishes acceptance; attaching to the durable workflow
establishes the observed terminal. Acceptance does not count as completion.
Timeouts, cancellations, failures and unresolved work remain separate
populations. First visible output and the remote settlement instant are
explicitly unavailable through this workflow endpoint. No remote timestamp is
subtracted from the client clock. Samples retain the collector's start/end
interval as clock uncertainty.

Histograms retain p50/p95/p99, queue delay, offered/completed rates and backlog
for each scenario and phase. Each outcome also has its own latency population.
The driver writes every raw record to `LASH_LOAD_MEASUREMENTS_PATH`. After
collection, it creates a `.complete` marker and stays alive while the controller
copies the file to `measurements.log`. The controller creates a `.collected`
marker, then waits for the Job to exit. Container logs contain progress only;
Kubernetes log rotation cannot discard the result archive. The acknowledgement
has the workload's drain timeout.

The report must match the driver's population and independent witness counts,
including the planned primary-turn population. Any duplicate, missing terminal,
invalid clock order, unresolved operation or failed witness fails the command.
The archive retains the exact independent event, provider receipt and effect
ledger rows before deleting the topology. The reconciler checks their identities,
requests, responses and timestamps against client records. Failed analyses retain
the raw rows, collection settings and witness verdict.

The invocation census starts before admission in this isolated topology, which
has one load driver and no concurrent traffic after the L2 smoke drains. All
new invocations belong to the run, including HTTP-started bodies without a
Restate parent ID. Journal aggregates read `raw_length`, retain per-invocation
maxima and growth, and include every retained completed invocation. Every outer
terminal also triggers a journal read before retention. Physical collectors
mount each Restate data volume read-only and record allocated bytes by
directory. They list the separate S3 snapshot prefix, including pagination.
The report counts that shared prefix once.

For this isolated evidence topology, `load.journalRetention` is fixed at `5m`
and applied to every registered service before load admission. This deliberately
differs from default deployment settings. `collection.json` records the before/after
service settings. L6 must preserve this flag. The eventual FIG-4172 baseline
campaign must decide its retention settings explicitly before comparing costs.
The driver allows up to 20 seconds for asynchronous internal cleanup after the
public operations finish; unfinished internal invocations fail reconciliation.
The driver deletes each actor's final session after its last turn completes,
as well as sessions retired by rotation or scripted deletion.
Session deletion first enumerates model-created processes through the public
provenance filter, cancels all live children and awaits their terminals. A turn
cancel drops its wait and does not cancel its parked child, so the workload must
do this cleanup explicitly. An incomplete census retains its metric/histogram
outputs with `qualification.status=INCOMPLETE` and exits unsuccessfully. Missing
or vanished internal completions are never excused as inferred cancellations.
The collector retains run-owned `sys_journal` signal commands with their exact
target invocation IDs. A disappeared inboxed `EffectGroupIndex` call is explained
only by a recorded built-in cancel signal targeting that ID, matching Restate
1.7.12's cancel-before-start contract. Such calls need no completed journal.
Unexplained disappearances fail qualification when the cancellation census is
available; older evidence without it remains `INCOMPLETE`.

`/load/resources` reads the worker and all descendant processes, preserving PID
start epochs, CPU, RSS and peak RSS. The report validates the parent/child sums.
Deployment memory is the sum of distinct worker cgroup memory values; process
RSS is a separate diagnostic. Cgroup CPU, throttling and OOM counters retain
their process epoch. The runbook shares one host-owned worker service across
its cells, compiler operations and durable bodies. The endpoint adds the actual
pool exporter to the independent process/cgroup snapshot.

Pool samples retain the deployment node, worker generation, parent start epoch
and pool epoch. They include worker/idle/queue occupancy, queued bytes, cumulative
queue waits and nanoseconds of queue delay, successfully transmitted and received
framed IPC messages/bytes including handshakes and resets, clean reset/reuse counts,
supervisor-observed EOF/crash counts, discards and successful replacement counts.
A replacement includes a discarded worker's successor even when checkout starts
it lazily. Guest errors, limits and cancellation are discards, with separate
crash counts. Every metric declares its unit. An exporter-backed zero is valid;
a missing counter is never inferred as zero.

The load worker retains at most 65,536 execution receipts. A receipt records the
first acknowledged compute phase of a lease, its cell/process class from the
explicit program entry, owner, worker PID and Linux start epoch. Pure preparation
is counted as checkout/IPC work and cannot supply either execution class. The
collector reconciles the counters with the receipts, matches run-owned session
and process IDs to workload responses, and matches worker PIDs/start epochs to
independent descendant samples. It archives run-owned receipts in
`pool_executions.jsonl`, with `resource_sampled` marking each match. Short-lived
children can finish between samples; their receipts remain explicitly unsampled
and cannot supply the required independent execution proof. Raw pool evidence
remains in `samples.jsonl` and per-epoch measurements appear in `metrics.jsonl`.

`summary.pool.status` is `PASSED` only with complete, reconciled measurements and
at least one independently sampled run-owned cell and durable body. Missing
metrics, either missing sampled execution class, receipt overflow or an unexplained
parent/pool restart interval makes qualification `INCOMPLETE` and the checker
exits unsuccessfully. A restart follows the counter epoch rule below: it is a
`counter_gap` of `pool_identity` in `collection_gaps.jsonl`, with both identities,
the counters on each side and an unknown delta. When its unobserved interval
meets exactly one window of a fault on that worker, it is `FAULT_ATTRIBUTED` and
the pool can still pass; `summary.pool` then reports `fault_attributed_epoch_gaps`
and `complete: false`. A restart that no fault, or more than one, could explain
stays `UNATTRIBUTED` and incomplete. Fault attribution and the separate recovery report remain
available. Overall qualification cannot pass while pool qualification is
incomplete. These checks establish collection completeness, with no latency,
saturation or release budget.

PostgreSQL enables `pg_stat_statements` and I/O timing for the load topology.
The collector records transactions, WAL, connections, I/O time/block counts,
current waits/lock waits and query calls/time. It normalizes deltas by durable
terminal operations. WAL has server scope, including the separate witness
database; query/transaction/I/O statistics have Lash database scope and include
collection overhead. PostgreSQL does not expose cumulative per-lock wait time;
that metric is explicitly unavailable rather than zero.

Restate retryable failures and suspensions use cumulative task counters and
node-generation epochs. Samples retain the partition's leader identity and
epoch alongside each counter. Leadership changes do not reset the node's
cumulative counters. The collector reads `sys_invocation_status` directly because the combined
`sys_invocation` view joins ephemeral leader state, which can disappear during
rebalancing. SQL `retry_count` resets at leadership changes and suspension, so
it is never used as a cumulative count. New counter series begin at zero when absent from the
initial scrape. An unexplained reset or an unobserved node-generation gap fails
measurement completeness. Failed scrape intervals are retained and the collector
continues, so a fault does not stop later evidence collection. Transient partition
transfer failures and interrupted read-only query connections get bounded retries
within the sample; every attempt is
retained in `query_retries.jsonl`, and the collection interval exposes the delay.
Provider retryable receipts, client attempts and
effect attempts/commits have independent counts.

The reconciler consumes the run's independent witness fault ledger and its
driver-recorded clock anchors. Normalized recovery inputs retain the injection,
service progress and backlog recovery intervals, accepted operation IDs and the
durability verdict. The recovery report gives separate service-progress and
full-backlog durations with error bars. The default recipe exercises this path
under the L4 fault campaign; the L5 setting disables faults for collection-only
smoke checks.

Run the focused measurement-law tests through Kiln:

```sh
kiln gate lash fig-4170 -- python3 scripts/test_loadtest_measurements.py
```

The pinned counter and journal meanings follow the
[Restate 1.7.12 invoker counters](https://github.com/restatedev/restate/blob/v1.7.12/crates/invoker-impl/src/metric_definitions.rs),
[journal schema](https://github.com/restatedev/restate/blob/v1.7.12/crates/storage-query-datafusion/src/journal/schema.rs)
and [retry snapshot schema](https://github.com/restatedev/restate/blob/v1.7.12/crates/storage-query-datafusion/src/invocation_state/schema.rs).

For the small L5 collection smoke, run `LASH_LOADTEST_FAULT_CAMPAIGN=false kiln gate lash fig-4170 -- just multi-node-load local`. The default recipe preserves the L4 fault campaign. Campaigns retain the extra four fault witness classes, all six independent ledgers, and primary turns beyond the planned minimum. The driver records a `campaign-start` anchor and one anchor for each fault ledger event, including each injection. Each read pairs `witness_clock_us()` with the midpoint of the driver's monotonic request/response interval and retains the full round-trip duration. The archive reads the independent `witness_load_faults` ledger directly, preserves it as `faults.jsonl`, and writes `clock_anchors.jsonl` and normalized `recovery_inputs.jsonl`. The driver hands off recovery evidence before its final metric census. The separate `recovery.json` and `normalized_faults.jsonl` retain recovery qualification and bounded measurements even if another collection gate fails, such as an intentional restart that gaps a process counter. The final census uses ordered pages of 128 invocation IDs and journal chunks of 64 owned IDs, retaining the existing ten-second request and retry bounds. A final census failure keeps the overall run failed.

A DataFusion `No such scanner` response restarts that SQL query once with a fresh POST from the beginning, inside its existing ten-second deadline. The retry record preserves the query kind, attempt and scanner error. No scanner identity or partial response carries into the fresh query. A second lost scanner fails the sample. Invocation pages and journal chunks remain separate bounded SQL queries.

Every sample error preserves its failing component and endpoint, observation timestamp, and whether it was periodic or final. Each injected fault records its affected collection targets. `collection_gaps.jsonl` and the summary list every gap with `FAULT_ATTRIBUTED` or `UNATTRIBUTED`. Attribution requires a periodic failure on a recorded target of the faulted component, observed inside the normalized fault window. The accepted window starts at the injection interval's upper bound and ends at the recovered event interval's lower bound. A worker kill and a Restate restart signal the process and then write the `injected` row, so the fault is live between the two. The controller therefore reads the witness clock before it signals and records it as `signal_not_before_us` in the injection. When present, that reading's upper bound opens the window instead. The attribution retains the fault ID, both anchor IDs and the window. Missing anchors, unknown targets, other endpoints, clock-boundary uncertainty, out-of-window errors and final census failures remain unattributed and fail the run. Attributed gaps stay absent from measured deltas and histograms. Other metric completeness and durability gates still apply.

Counter epoch transitions follow the same target rule, but a transition happened at an unknown point after the last old-epoch observation and no later than the first new-epoch one. That unobserved interval must meet exactly one matching window, so a restart's reset first sampled after recovery is attributed, while an interval wholly before injection or after recovery is not. Worker resource and Restate metric reads retain their own observation timestamps, so a later query in the sample cannot move the transition's clock. Every transition records its counter identity, pre- and post-epoch values, and `unobserved_delta: null`. Its interval remains `complete: false`; observed deltas exclude the unknown interval. A matching fault can qualify that explicitly incomplete interval for the fault smoke. An unattributed epoch transition raises and fails the run, and its evidence remains in `collection_gaps.jsonl` and the failed summary. No missing delta is interpolated.

Normalization uses only the event's recorded anchor. Its absolute timestamp error bar is half the round trip, rounded up, plus one microsecond for witness precision. Recovery durations retain the difference of both endpoint intervals. A witness offset that moves beyond the start and event anchor bounds leaves recovery inputs INCOMPLETE. Missing anchors name the fault ID, phase and event ID. Every raw row must normalize before recovery inputs qualify COMPLETE.

The accepted population includes all inputs accepted through the injection interval's upper bound. Every one must reach a durable terminal by the backlog recovery interval's upper bound. The controller records absolute witness microseconds for first service progress and for the observation where its full recovery conditions hold. The existing stable hold and durability verdict remain required. These bounds qualify the collection; they establish no performance budget.

A failed load run keeps its repro before the recipe deletes the namespace. `scripts/loadtest_repro.py` writes `raw-journals/<utc>/` in the run directory. It contains both witness ledgers, every outer `E2eLoadWorkflow` invocation and every open invocation with their journals. For each affected session it also writes `session-<n>-invocations.json`, `session-<n>-open.json` and `session-<n>-journal-<g>.json`: that session's `LashSession` and `LashTurn` invocations, the open ones apart, and all their journals. A session is affected when a witness violation, a driver diagnosis or a failed fault names one of its operations. The witness events map the operation to its session, and the session's turns are the `LashTurn` keys that start with `{len}:{session}`. `complete.json` lists the sessions, and `repro-capture.log` records the attempt. A green run skips the capture.

## PostgreSQL capacity

`postgres.maxConnections` must cover
`workers.count * (workers.pgConnections + workers.witnessConnections) * postgres.maxGenerations + postgres.otherWorkers + postgres.adminHeadroom`.
The default two-generation roll needs 94 connections. A rollback campaign
uses three generations and needs 130. `scripts/multi-node-load.sh` derives the
server setting with `scripts/loadtest_connection_budget.py::peak_connections`.
The chart rejects a peak above the declaration and runs `lashctl preflight`
against the live server before upgrades. Witness pools are configured to two
connections per process. The other clients and reserved-slot allocation are
listed in the [rolling runbook](../../../runbooks/rolling-upgrade/runbook.md#postgresql-connection-budget).
