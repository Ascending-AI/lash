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

The foreground recipe builds three binaries on the shared pool, creates their
runtime image, boots a three-node kind cluster, and installs this chart. It
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
multi-node-load` applies it after the topology gates, waits for it, archives
its log as `load.log` and `load-witness.txt`, and fails unless the driver
printed `load witness verdict=passed`. `load.workload` names the checked-in
workload every worker, the provider and the driver generate from (`smoke-v1`
or `figments-v1`); `load.sessions` and `load.turnsPerSession` bound the run.
The default is the pipeline smoke: four sessions of six turns, a minute or two.
It is not a baseline and derives no budgets.

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

The `<name>-faults` ConfigMap exports `hooks.json` from values and `targets.json`
with the namespace, selected worker Deployment, selected Restate pod, generation
and deployment URL. L4 uses these to target a busy worker, restart a Restate node
without deleting its PVC, and consume `faults.rollingDeploy.trigger`. L4 owns the
actual fault timestamps, busy-work check, restart delay and recovery witnesses.
Changing this trigger signals that controller; it does not replace a worker at
an existing Restate deployment URL.

For a replacement generation, set `workers.generation`, retain the old name in
`workers.retainedGenerations`, and record both immutable tags in
`workers.generationImages`. Keep `image.tag` at the original bootstrap image:
choose worker builds through `workers.generationImages` so a worker replacement
does not roll the Restate, PostgreSQL or Garage link-shaping init containers. Every generation gets independent worker Deployments
and a proxy Service named `<name>-workers-<generation>`. A retained generation
must have an explicit tag. Register the new endpoint, move admission and verify
generation drain plus the Restate pinned-invocation checks before removing the
old generation from values. Helm alone does not perform that protocol.

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
