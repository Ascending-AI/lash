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
kiln gate lash fig-4167 -- just multi-node-load
```

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

The evidence lives in the witness database, outside lash's store: the driver's
sent and terminal rows, the provider's receipts, the synthetic tools' effect
attempts and commits, and the exact blob bytes put and read (digested by the
witness). The driver then reconciles 19 evidence classes against the plan the
workload regenerates and prints one `load witness class=...` line per class.
Any violation, and any class with no evidence, fails the run. The final result
archive and measurements belong to later lanes.

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
   before serving at the identical endpoint.
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
