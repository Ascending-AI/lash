#!/usr/bin/env bash
set -euo pipefail

main() {
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
: "${KILN_GATE_ID:?run through kiln gate lash <fork> -- just multi-node-load}"
bash scripts/ensure-loadtest-tools.sh kind
export PATH="$repo/target/loadtest-tools:$PATH"
for tool in helm kind kubectl docker; do command -v "$tool" >/dev/null; done
name="$(printf '%s' "$KILN_GATE_ID" | tr '[:upper:]_' '[:lower:]-' | cut -c1-40)"
namespace="$name"
run="$repo/target/fig-3790/$name-$(date -u +%Y%m%dT%H%M%SZ)"
mkdir -p "$run" target/loadtest-image/bin
if [[ "${1:-}" != --locked ]]; then
  exec flock --nonblock --close "$repo/target/loadtest-tools/$name.run.lock" bash "${BASH_SOURCE[0]}" --locked "$@"
fi
shift
target="${1:-local}"
# The campaign the controller runs against the load: the FIG-4169 fault
# campaign, or the FIG-3805 rolling upgrade (N to the synthetic N+1 with a
# rollback leg, finalize and the stale-writer fence).
campaign="${2:-faults}"
if [[ $# -gt 2 ]] || [[ "$target" != local && "$target" != scaleway ]] || \
    [[ "$campaign" != faults && "$campaign" != rolling-upgrade ]]; then
  echo "usage: multi-node-load.sh [local|scaleway] [faults|rolling-upgrade]" >&2; exit 2
fi
if [[ "$target" == scaleway ]]; then
  echo "the scaleway load target is PENDING: it needs a provisioned Scaleway Kapsule cluster," >&2
  echo "an image registry the cluster can pull from, and operator-supplied credentials" >&2
  echo "(deploy/helm/lash-loadtest/README.md). Local runs use target 'local'." >&2
  exit 2
fi
if kind get clusters | grep -Fx "$name"; then echo "refusing to reuse cluster $name" >&2; exit 1; fi
# All calls name this run's kubeconfig. Never change the user's context.
chart=deploy/helm/lash-loadtest
profile="${LASH_LOADTEST_VALUES:-$chart/values-local.yaml}"
python3 - "$chart/values.yaml" "$profile" "$run" "$name" "$campaign" <<'PYVALUES'
import json, pathlib, sys, yaml
sys.path.insert(0, 'scripts')
from loadtest_connection_budget import peak_connections
base, profile, directory, tag, campaign = sys.argv[1:]
values = yaml.safe_load(open(base))
def merge(target, overlay):
    for key, value in overlay.items():
        if isinstance(value, dict) and isinstance(target.get(key), dict):
            merge(target[key], value)
        else:
            target[key] = value
merge(values, yaml.safe_load(open(profile)))
if (values['restate']['replicas'] != 3 or values['restate']['partitions'] != 24
        or values['restate']['replication'] != 2):
    raise SystemExit('the v1 topology proof requires three Restate nodes, 24 partitions and replication two')
if values['s3']['mode'] != 'garage':
    raise SystemExit('the local proof uses run-owned Garage storage')
if campaign == 'rolling-upgrade':
    # The rollback and the roll each serve three worker generations at once.
    values['postgres']['maxGenerations'] = 3
values['postgres']['maxConnections'] = peak_connections(values)
values['image']['tag'] = tag
values['driver']['enabled'] = False
values['load']['enabled'] = False
path = pathlib.Path(directory)
(path/'run-values.yaml').write_text(yaml.safe_dump(values))
(path/'kind.yaml').write_text(yaml.safe_dump({
    'kind': 'Cluster', 'apiVersion': 'kind.x-k8s.io/v1alpha4',
    'nodes': [{'role':'control-plane'}] + [{'role':'worker'}] * (values['local']['nodes'] - 1),
}))
(path/'build-settings.json').write_text(json.dumps({
    'runtime': values['image']['runtimeBase'], 'nodeImage': values['local']['nodeImage'],
    'repository': values['image']['repository'], 'name': values['nameOverride'],
    'secret': values['credentialsSecret'], 'workers': values['workers']['count'],
    'generation': values['workers']['generation'],
    'loadDeadline': values['load']['activeDeadlineSeconds'],
    'workload': values['load']['workload'],
    'restateImage': values['restate']['image'],
}))
PYVALUES
values=(-f "$run/run-values.yaml")
readarray -t settings < <(python3 - "$run/build-settings.json" <<'PYSETTINGS'
import json, sys
settings = json.load(open(sys.argv[1]))
for key in ['name', 'secret', 'workers', 'generation', 'loadDeadline', 'workload']:
    print(settings[key])
PYSETTINGS
)
resource="${settings[0]}"
secret="${settings[1]}"
workers="${settings[2]}"
generation="${settings[3]}"
load_deadline="${settings[4]}"
workload="${settings[5]}"
# Source and host provenance for the results manifest (FIG-4171).
python3 - "$run/sources.json" <<'PYSOURCES'
import json, subprocess, sys
json.dump({'lash_sha': subprocess.check_output(['git', 'rev-parse', 'HEAD'], text=True).strip(),
           'lash_dirty': bool(subprocess.check_output(['git', 'status', '--porcelain'], text=True).strip())},
          open(sys.argv[1], 'w'), indent=1)
PYSOURCES
python3 - "$run/hardware.json" "$repo" <<'PYHARDWARE'
import json, os, platform, shutil, sys
memory = int(next(line.split()[1] for line in open('/proc/meminfo') if line.startswith('MemTotal')))
json.dump({'arch': platform.machine(), 'logical_cpus': os.cpu_count(), 'memory_kib': memory,
           'kernel': platform.release(), 'workspace_capacity_bytes': shutil.disk_usage(sys.argv[2]).total},
          open(sys.argv[1], 'w'), indent=1)
PYHARDWARE
export KUBECONFIG="$run/kubeconfig"
k=(kubectl --kubeconfig "$KUBECONFIG" --namespace "$namespace")
created=0
image_id=""
next_image_id=""
cleanup() {
  status=$?
  if ((created)); then
    "${k[@]}" get pods,pvc,jobs -o wide > "$run/resources.txt" 2>&1 || true
    "${k[@]}" get events --sort-by=.lastTimestamp > "$run/events.txt" 2>&1 || true
    for pod in $("${k[@]}" get pods -o name 2>/dev/null); do
      "${k[@]}" logs "$pod" --all-containers --prefix > "$run/${pod#pod/}.log" 2>&1 || true
    done
    "${k[@]}" delete namespace "$namespace" --wait=true --timeout=120s > "$run/namespace-cleanup.log" 2>&1 || status=1
    if kubectl --kubeconfig "$KUBECONFIG" get namespace "$namespace" >/dev/null 2>&1; then status=1; fi
    kind delete cluster --name "$name" > "$run/cluster-cleanup.log" 2>&1 || status=1
    if kind get clusters | grep -Fx "$name"; then status=1; fi
  fi
  rm -f "$run/image.tar"
  if [[ -n "$image_id" ]] && [[ "$(docker image inspect "$image" --format '{{.Id}}' 2>/dev/null || true)" == "$image_id" ]]; then
    docker image rm "$image" > "$run/image-cleanup.log" 2>&1 || status=1
  fi
  if [[ -n "$next_image_id" ]] && [[ "$(docker image inspect "$next_image" --format '{{.Id}}' 2>/dev/null || true)" == "$next_image_id" ]]; then
    docker image rm "$next_image" >> "$run/image-cleanup.log" 2>&1 || status=1
  fi
  printf 'topology_exit=%s evidence=%s\n' "$status" "$run"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
binaries=(lash-e2e-worker lash-e2e-mock-provider lash-loadtest-smoke lash-loadtest-driver)
labels=("//crates/lashctl:lashctl")
for binary in "${binaries[@]}"; do labels+=("//runbooks/restate-postgres-workers:${binary}__bin"); done
# The rolling deploy's replacement generation (FIG-4169): the same worker and
# its operator binary built as the synthetic N+1, resolved from the generated
# feature inventory so a feature-set change moves no recipe.
next_worker="$(python3 scripts/resolve_buck2_target.py \
  //runbooks/restate-postgres-workers lash-e2e-worker__bin --feature synthetic-next)"
next_lashctl="$(python3 scripts/resolve_buck2_target.py \
  //crates/lashctl lashctl --feature synthetic-next)"
# Model code runs only in the VM helper beside the worker. Ship each
# generation with the helper selected from that worker's actual feature
# closure, then materialize all binaries from one Buck2 build report.
helper_pair="$(python3 scripts/check_loadtest_cluster.py helper . //runbooks/restate-postgres-workers:lash-e2e-worker__bin)"
next_helper_pair="$(python3 scripts/check_loadtest_cluster.py helper . "$next_worker")"
read -r helper helper_testing <<< "$helper_pair"
read -r next_helper next_helper_testing <<< "$next_helper_pair"
labels+=("$next_worker" "$next_lashctl" "$helper" "$next_helper")
build_report="target/loadtest-image/build-report.json"
kiln build --materializations final --build-report "$build_report" "${labels[@]}"
output() {
  python3 tools/buck2/outputs.py --report "$build_report" --label "$1" --single
}
install -m 755 "$(output //crates/lashctl:lashctl)" target/loadtest-image/bin/lashctl
rm -rf target/loadtest-image/bin-next
mkdir -p target/loadtest-image/bin-next
for binary in "${binaries[@]}"; do
  label="//runbooks/restate-postgres-workers:${binary}__bin"
  install -m 755 "$(output "$label")" "target/loadtest-image/bin/$binary"
  install -m 755 "$(output "$label")" "target/loadtest-image/bin-next/$binary"
done
install -m 755 "$(output "$next_worker")" target/loadtest-image/bin-next/lash-e2e-worker
install -m 755 "$(output "$next_lashctl")" target/loadtest-image/bin-next/lashctl
install -m 755 "$(output "$helper")" target/loadtest-image/bin/lash-vm-worker
install -m 755 "$(output "$next_helper")" target/loadtest-image/bin-next/lash-vm-worker
python3 scripts/check_loadtest_cluster.py image target/loadtest-image/bin "$helper_testing" > "$run/vm-helper.txt"
python3 scripts/check_loadtest_cluster.py image target/loadtest-image/bin-next "$next_helper_testing" >> "$run/vm-helper.txt"
# The chart's schema Job creates the witness role/database using secret values.
sed '/^CREATE ROLE lash_witness /d; /^CREATE DATABASE lash_witness /d' runbooks/restate-postgres-workers/witness.sql > target/loadtest-image/witness.sql
image="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["repository"])' "$run/build-settings.json"):$name"
runtime_base="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["runtime"])' "$run/build-settings.json")"
node_image="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["nodeImage"])' "$run/build-settings.json")"
next_image="$image-next"
restate_image="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["restateImage"])' "$run/build-settings.json")"
docker build --build-arg "RUNTIME_BASE=$runtime_base" --build-arg "RESTATE_IMAGE=$restate_image" -t "$image" -f deploy/helm/lash-loadtest/Dockerfile . > "$run/image-build.log" 2>&1
image_id="$(docker image inspect "$image" --format '{{.Id}}')"
docker build --build-arg "RUNTIME_BASE=$runtime_base" --build-arg "RESTATE_IMAGE=$restate_image" --build-arg BIN_DIR=target/loadtest-image/bin-next \
  -t "$next_image" -f deploy/helm/lash-loadtest/Dockerfile . >> "$run/image-build.log" 2>&1
next_image_id="$(docker image inspect "$next_image" --format '{{.Id}}')"
python3 - "$run/image-digests.json" "$image" "$image_id" "$next_image" "$next_image_id" \
  "$restate_image" "$runtime_base" "$node_image" <<'PYIMAGES'
import json, sys
out, image, image_id, next_image, next_image_id, restate, base, node = sys.argv[1:]
json.dump({'runtime': {'reference': image, 'id': image_id},
           'next_runtime': {'reference': next_image, 'id': next_image_id},
           'restate': restate, 'runtime_base': base, 'node_image': node},
          open(out, 'w'), indent=1)
PYIMAGES
created=1
env -u HTTP_PROXY -u HTTPS_PROXY -u ALL_PROXY -u http_proxy -u https_proxy -u all_proxy kind create cluster --name "$name" --image "$node_image" --config "$run/kind.yaml" --kubeconfig "$KUBECONFIG" --wait 120s > "$run/kind.log" 2>&1
load_image() {
  # Docker's containerd store retains multi-platform manifest indexes while
  # pulling only this host's layers. Import only the local platform, rather
  # than asking ctr to resolve every architecture in an incomplete index.
  docker save --platform linux/amd64 "$1" -o "$run/image.tar"
  for node in $(kind get nodes --name "$name"); do
    docker exec -i "$node" ctr --namespace=k8s.io images import \
      --platform linux/amd64 --digests --snapshotter=overlayfs - < "$run/image.tar"
  done
  rm "$run/image.tar"
}
load_image "$image" > "$run/image-load.log" 2>&1
load_image "$next_image" >> "$run/image-load.log" 2>&1
kubectl --kubeconfig "$KUBECONFIG" create namespace "$namespace"
# Ephemeral synthetic credentials are kept only in the run's cluster.
pg_password="$(openssl rand -hex 24)"
witness_password="$(openssl rand -hex 24)"
s3_key="GK$(openssl rand -hex 12)"
s3_secret="$(openssl rand -hex 32)"
rpc_secret="$(openssl rand -hex 32)"
"${k[@]}" create secret generic "$secret" \
  --from-literal="postgres-password=$pg_password" \
  --from-literal="witness-password=$witness_password" \
  --from-literal="database-url=postgres://lash:$pg_password@${resource}-postgres:5432/lash" \
  --from-literal="witness-database-url=postgres://lash_witness:$witness_password@${resource}-postgres:5432/lash_witness" \
  --from-literal="s3-access-key=$s3_key" --from-literal="s3-secret-key=$s3_secret" --from-literal="garage-rpc-secret=$rpc_secret"
unset pg_password witness_password s3_key s3_secret rpc_secret
helm lint "$chart" "${values[@]}" > "$run/helm-lint.log"
helm template topology "$chart" --namespace "$namespace" "${values[@]}" > "$run/topology.yaml"
helm install topology "$chart" --namespace "$namespace" "${values[@]}" > "$run/helm-install.log"
"${k[@]}" wait --for=condition=complete "job/${resource}-schema" --timeout=180s
"${k[@]}" rollout status "statefulset/${resource}-restate" --timeout=180s
ctl() { "${k[@]}" exec "${resource}-restate-0" -c restate -- restatectl "$@"; }
ctl provision --replication 2 --num-partitions 24 --yes > "$run/provision.log"
# Stable node names preserve IDs across pod restarts; node IDs come from
# committed metadata, never from assumptions about pod startup ordering.
for attempt in $(seq 1 90); do
  if ctl metadata get --key nodes_config > "$run/nodes.json" && \
      python3 scripts/check_loadtest_cluster.py nodes "$run/nodes.json" > "$run/node-ids.txt"; then break; fi
  sleep 2
done
python3 scripts/check_loadtest_cluster.py nodes "$run/nodes.json" > "$run/node-ids.txt"
while read -r node; do ctl metadata-server add-node "$node"; done < "$run/node-ids.txt"
ctl metadata-server list-servers > "$run/metadata-quorum.txt"
ctl status > "$run/cluster-status.txt"
for attempt in $(seq 1 90); do
  if ctl metadata get --key nodes_config > "$run/nodes.json" && \
      ctl metadata get --key bifrost_config > "$run/logs.json" && \
      ctl metadata get --key partition_table > "$run/partitions.json" && \
      python3 scripts/check_loadtest_cluster.py replication "$run/nodes.json" "$run/logs.json" "$run/partitions.json" > "$run/replication.txt"; then break; fi
  sleep 2
done
python3 scripts/check_loadtest_cluster.py replication "$run/nodes.json" "$run/logs.json" "$run/partitions.json" > "$run/replication.txt"
mkdir -p "$run/epochs"
for partition in $(seq 0 23); do ctl metadata get --key "pp_epoch_$partition" > "$run/epochs/$partition.json"; done
python3 scripts/check_loadtest_cluster.py placement "$run"/epochs/*.json > "$run/placement.txt"
for index in $(seq 0 "$((workers - 1))"); do "${k[@]}" rollout status "deployment/${resource}-worker-$index-${generation}" --timeout=180s; done
"${k[@]}" rollout status "deployment/${resource}-proxy-${generation}" --timeout=120s
"${k[@]}" get pods -o json > "$run/pods-placement.json"
python3 - "$run/placement.json" "$run/pods-placement.json" "$target" "$name" "$namespace" <<'PYPLACEMENT'
import json, subprocess, sys
out, pods_path, target, cluster, namespace = sys.argv[1:]
document = json.load(open(pods_path))
pods = {item['metadata']['name']: item['spec'].get('nodeName') for item in document['items']}
kind_nodes = subprocess.check_output(['kind', 'get', 'nodes', '--name', cluster], text=True).split()
json.dump({'target': target, 'cluster': f'kind:{cluster}', 'namespace': namespace,
           'nodes': sorted(kind_nodes), 'pods': pods}, open(out, 'w'), indent=1)
PYPLACEMENT
# Hold one node down while new durable traffic runs. A replicas=2 scale-down
# retains node 2's PVC/identity; the remaining two must commit fresh work.
# The rolling-upgrade campaign proves the upgrade, not quorum loss: it keeps
# all three nodes up and goes straight to the smoke registration.
if [[ "$campaign" == faults ]]; then
"${k[@]}" scale "statefulset/${resource}-restate" --replicas=2
"${k[@]}" wait --for=delete "pod/${resource}-restate-2" --timeout=120s
stable=0
previous=""
for attempt in $(seq 1 90); do
  if ctl status > "$run/quorum-status.txt" && \
      python3 scripts/check_loadtest_cluster.py availability "$run/nodes.json" "$run/quorum-status.txt" "$resource-restate-2" > "$run/availability.txt"; then
    current="$(cat "$run/availability.txt")"
    if [[ "$current" == "$previous" ]]; then stable=$((stable + 1)); else stable=1; fi
    previous="$current"
    if ((stable >= 2)); then break; fi
  else
    stable=0
    previous=""
  fi
  sleep 2
done
((stable >= 2))
cat "$run/availability.txt"
fi
printf 'driver:\n  enabled: true\n' > "$run/driver-values.yaml"
helm template topology "$chart" --namespace "$namespace" "${values[@]}" -f "$run/driver-values.yaml" \
  --show-only templates/jobs.yaml | python3 scripts/check_loadtest_cluster.py job smoke | "${k[@]}" apply -f -
"${k[@]}" wait --for=condition=complete "job/${resource}-smoke" --timeout=300s
"${k[@]}" logs "job/${resource}-smoke" -c smoke > "$run/smoke.log"
if [[ "$campaign" == faults ]]; then
ctl snapshots create --trim-log > "$run/snapshots.txt" 2>&1
if grep -E 'ERROR|Failed|failed' "$run/snapshots.txt"; then exit 1; fi
[[ $(grep -c 'Snapshot created for partition' "$run/snapshots.txt") -eq 24 ]]
"${k[@]}" scale "statefulset/${resource}-restate" --replicas=3
"${k[@]}" rollout status "statefulset/${resource}-restate" --timeout=180s
# Recovered means every node's own failure detector sees the restarted node
# alive: `ctl status` lists it earlier, and a census query it coordinates
# before then loses its scanners.
peer_views() {
  for index in 0 1 2; do
    "${k[@]}" exec "${resource}-restate-$index" -c restate -- restatectl sql --json \
      'SELECT plain_node_id, gen_node_id, name, state FROM nodes' > "$run/peers-$index.txt" || return 1
  done
}
peers=("$run/peers-0.txt" "$run/peers-1.txt" "$run/peers-2.txt")
for attempt in $(seq 1 90); do
  if ctl status > "$run/recovered-status.txt" && \
      python3 scripts/check_loadtest_cluster.py recovery "$run/nodes.json" "$run/recovered-status.txt" "$resource-restate-2" > "$run/recovery.txt" && \
      peer_views && python3 scripts/check_loadtest_cluster.py peers "$run/nodes.json" "$resource-restate-2" "${peers[@]}" >> "$run/recovery.txt"; then break; fi
  sleep 2
done
python3 scripts/check_loadtest_cluster.py recovery "$run/nodes.json" "$run/recovered-status.txt" "$resource-restate-2"
python3 scripts/check_loadtest_cluster.py peers "$run/nodes.json" "$resource-restate-2" "${peers[@]}"
fi
for attempt in $(seq 1 60); do
  "${k[@]}" exec "deployment/${resource}-proxy-${generation}" -- wget -qO- "http://${resource}-metrics:9090/api/v1/targets" > "$run/metrics-targets.json"
  if python3 scripts/check_loadtest_cluster.py metrics "$run/metrics-targets.json" > "$run/metrics.txt"; then break; fi
  sleep 2
done
python3 scripts/check_loadtest_cluster.py metrics "$run/metrics-targets.json"
if [[ "$campaign" == faults ]]; then
  printf 'topology gates passed: restate_nodes=3 metadata_members=3 replication=2 public_turns=1 peer_reads=1 quorum_nodes_unavailable=1\n' | tee "$run/result.txt"
else
  printf 'topology gates passed: restate_nodes=3 metadata_members=3 replication=2 public_turns=1 peer_reads=1\n' | tee "$run/result.txt"
fi
# The durable workload (FIG-4168) under the fault campaign (FIG-4169): every
# public durable operation class keeps running while the controller kills a
# busy worker, restarts a Restate leader and rolls the workers to the
# synthetic N+1 with a generation drain. The driver reconciles the witness
# ledgers, fault classes included, once the campaign ends.
# The rolling-upgrade campaign (FIG-3805 phase B) runs the ADR 0106 §6
# choreography instead, under the same load; its steps are the ledger's
# `half-roll`, `rollback`, `roll`, `finalize` and `fence` rows.
fault_campaign="${LASH_LOADTEST_FAULT_CAMPAIGN:-true}"
[[ "$fault_campaign" == true || "$fault_campaign" == false ]] || { echo "LASH_LOADTEST_FAULT_CAMPAIGN must be true or false" >&2; exit 1; }
if [[ "$campaign" == rolling-upgrade && "$fault_campaign" != true ]]; then
  echo "the rolling-upgrade campaign needs the load to run until its campaign ends" >&2; exit 1
fi
load_run="$workload-$(date -u +%Y%m%d%H%M%S)"
# The rolling upgrade's five steps each wait out a Helm rollout, a drain and
# every session answering; its load gets a longer deadline than a fault run.
if [[ "$campaign" == rolling-upgrade ]]; then load_deadline=1800; fi
printf 'load:\n  enabled: true\n  faultCampaign: %s\n  run: %s\n  activeDeadlineSeconds: %s\n' \
  "$fault_campaign" "$load_run" "$load_deadline" > "$run/load-values.yaml"
helm template topology "$chart" --namespace "$namespace" "${values[@]}" -f "$run/load-values.yaml" \
  --show-only templates/jobs.yaml | python3 scripts/check_loadtest_cluster.py job load | "${k[@]}" apply -f -
"${k[@]}" rollout status "deployment/${resource}-fault-probe" --timeout=120s
campaign_status=0
if [[ "$campaign" == rolling-upgrade ]]; then
python3 scripts/loadtest_upgrade.py --kubeconfig "$KUBECONFIG" --namespace "$namespace" --name "$resource" \
  --run "$load_run" --workload "crates/lash-perf/workloads/$workload.json" --workload-name "$workload" \
  --run-dir "$run" --chart "$chart" --values "$run/run-values.yaml" \
  --initial-tag "$name" --next-tag "$name-next" 2>&1 | tee "$run/upgrade.log" || campaign_status=$?
elif [[ "$fault_campaign" == true ]]; then
python3 scripts/loadtest_faults.py --kubeconfig "$KUBECONFIG" --namespace "$namespace" --name "$resource" \
  --run "$load_run" --workload "crates/lash-perf/workloads/$workload.json" --workload-name "$workload" \
  --run-dir "$run" --chart "$chart" --values "$run/run-values.yaml" \
  --initial-tag "$name" --next-tag "$name-next" 2>&1 | tee "$run/faults.log" || campaign_status=$?
fi
load_state=""
load_collected=0
recovery_collected=0
results_root="${LASH_LOADTEST_RESULTS:-$run/results}"
for attempt in $(seq 1 "$load_deadline"); do
  load_state="$("${k[@]}" get "job/${resource}-load" -o jsonpath='{.status.succeeded}/{.status.failed}')"
  if ((recovery_collected == 0)) && "${k[@]}" exec "job/${resource}-load" -c load -- test -f /tmp/load-measurements.recovery-ready >/dev/null 2>&1; then
    "${k[@]}" exec "job/${resource}-load" -c load -- cat /tmp/load-measurements.jsonl > "$run/recovery-measurements.log"
    recovery_status=0
    # Fault recovery qualification reads the fault campaign's kinds; the
    # rolling upgrade's steps are judged by the driver's witness verdict.
    if [[ "$campaign" == faults ]]; then
      python3 scripts/loadtest_measurements.py "$run/recovery-measurements.log" "$results_root" --recovery-only || recovery_status=$?
    fi
    "${k[@]}" exec "job/${resource}-load" -c load -- touch /tmp/load-measurements.recovery-collected
    recovery_collected=1
    if ((recovery_status != 0)); then campaign_status=$recovery_status; fi
  fi
  if "${k[@]}" exec "job/${resource}-load" -c load -- test -f /tmp/load-measurements.complete >/dev/null 2>&1; then
    "${k[@]}" exec "job/${resource}-load" -c load -- cat /tmp/load-measurements.jsonl > "$run/measurements.log"
    "${k[@]}" exec "job/${resource}-load" -c load -- touch /tmp/load-measurements.collected
    load_collected=1
    for finish in $(seq 1 60); do
      load_state="$("${k[@]}" get "job/${resource}-load" -o jsonpath='{.status.succeeded}/{.status.failed}')"
      if [[ "$load_state" == 1/* || "$load_state" == */1 ]]; then break; fi
      sleep 1
    done
    break
  fi
  if [[ "$load_state" == */1 ]]; then break; fi
  sleep 1
done
"${k[@]}" logs "job/${resource}-load" -c load > "$run/load.log"
if [[ "$campaign" == rolling-upgrade ]]; then
  # The upgrade proof is the witness verdict; it takes no measurement
  # archive, which belongs to the FIG-3790 baseline.
  grep '^load \|^load witness' "$run/load.log" > "$run/load-witness.txt" || true
  ((campaign_status == 0))
  grep -F 'load witness verdict=passed' "$run/load.log"
  [[ "$load_state" == 1/* ]]
  printf 'rolling upgrade passed: %s\n' "$(grep -F 'load witness verdict=passed' "$run/load.log")" | tee -a "$run/result.txt"
  exit 0
fi
measure_status=0
if ((load_collected == 1)); then
  python3 scripts/loadtest_measurements.py "$run/measurements.log" "$results_root" || measure_status=$?
else
  python3 - "$results_root/fig-3790/$load_run" <<'PYFAILED'
import json, pathlib, sys
output = pathlib.Path(sys.argv[1])
if output.is_dir():
    (output / 'summary.json').write_text(json.dumps({'schema_version': 1, 'verdict': 'failed',
        'error': 'driver exited before final metric census completed; recovery evidence is retained separately'}) + '\n')
PYFAILED
  measure_status=1
fi
# Package retained evidence even when the overall qualification fails.
if [[ -d "$results_root/fig-3790/$load_run" ]]; then
  python3 scripts/loadtest_manifest.py --run-dir "$run" --results "$results_root" \
    --run-id "$load_run" --target "$target" --workload "crates/lash-perf/workloads/$workload.json"
fi
((measure_status == 0))
grep '^load \|^load witness' "$run/load.log" > "$run/load-witness.txt" || true
((campaign_status == 0))
grep -F 'load witness verdict=passed' "$run/load.log"
[[ "$load_state" == 1/* ]]
printf 'durable workload passed: fault_campaign=%s %s\n' "$fault_campaign" "$(grep -F 'load witness verdict=passed' "$run/load.log")" | tee -a "$run/result.txt"
printf 'results archive: %s\n' "$results_root/fig-3790/$load_run" | tee -a "$run/result.txt"
}

main "$@"; exit "$?"
