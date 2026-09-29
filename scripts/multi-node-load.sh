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
if kind get clusters | grep -Fx "$name"; then echo "refusing to reuse cluster $name" >&2; exit 1; fi
# All calls name this run's kubeconfig. Never change the user's context.
chart=deploy/helm/lash-loadtest
profile="${LASH_LOADTEST_VALUES:-$chart/values-local.yaml}"
python3 - "$chart/values.yaml" "$profile" "$run" "$name" <<'PYVALUES'
import json, pathlib, sys, yaml
base, profile, directory, tag = sys.argv[1:]
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
values['image']['tag'] = tag
values['driver']['enabled'] = False
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
}))
PYVALUES
values=(-f "$run/run-values.yaml")
readarray -t settings < <(python3 - "$run/build-settings.json" <<'PYSETTINGS'
import json, sys
settings = json.load(open(sys.argv[1]))
for key in ['name', 'secret', 'workers', 'generation']:
    print(settings[key])
PYSETTINGS
)
resource="${settings[0]}"
secret="${settings[1]}"
workers="${settings[2]}"
generation="${settings[3]}"
export KUBECONFIG="$run/kubeconfig"
k=(kubectl --kubeconfig "$KUBECONFIG" --namespace "$namespace")
created=0
image_id=""
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
  printf 'topology_exit=%s evidence=%s\n' "$status" "$run"
  exit "$status"
}
trap cleanup EXIT
trap 'exit 130' INT
trap 'exit 143' TERM
labels=(//runbooks/restate-postgres-workers:lash-e2e-worker__bin //runbooks/restate-postgres-workers:lash-e2e-mock-provider__bin //runbooks/restate-postgres-workers:lash-loadtest-smoke__bin)
kiln build --remote_download_outputs=toplevel "${labels[@]}"
for binary in lash-e2e-worker lash-e2e-mock-provider lash-loadtest-smoke; do
  install -m 755 "bazel-bin/runbooks/restate-postgres-workers/${binary}__bin" "target/loadtest-image/bin/$binary"
done
# The chart's schema Job creates the witness role/database using secret values.
sed '/^CREATE ROLE lash_witness /d; /^CREATE DATABASE lash_witness /d' runbooks/restate-postgres-workers/witness.sql > target/loadtest-image/witness.sql
image="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["repository"])' "$run/build-settings.json"):$name"
runtime_base="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["runtime"])' "$run/build-settings.json")"
node_image="$(python3 -c 'import json, sys; print(json.load(open(sys.argv[1]))["nodeImage"])' "$run/build-settings.json")"
docker build --build-arg "RUNTIME_BASE=$runtime_base" -t "$image" -f deploy/helm/lash-loadtest/Dockerfile . > "$run/image-build.log" 2>&1
image_id="$(docker image inspect "$image" --format '{{.Id}}')"
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
ctl() { "${k[@]}" exec "${resource}-restate-0" -- restatectl "$@"; }
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
# Hold one node down while new durable traffic runs. A replicas=2 scale-down
# retains node 2's PVC/identity; the remaining two must commit fresh work.
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
printf 'driver:\n  enabled: true\n' > "$run/driver-values.yaml"
helm template topology "$chart" --namespace "$namespace" "${values[@]}" -f "$run/driver-values.yaml" \
  --show-only templates/jobs.yaml | python3 scripts/check_loadtest_cluster.py smoke-job | "${k[@]}" apply -f -
"${k[@]}" wait --for=condition=complete "job/${resource}-smoke" --timeout=300s
"${k[@]}" logs "job/${resource}-smoke" -c smoke > "$run/smoke.log"
ctl snapshots create --trim-log > "$run/snapshots.txt" 2>&1
if grep -E 'ERROR|Failed|failed' "$run/snapshots.txt"; then exit 1; fi
[[ $(grep -c 'Snapshot created for partition' "$run/snapshots.txt") -eq 24 ]]
"${k[@]}" scale "statefulset/${resource}-restate" --replicas=3
"${k[@]}" rollout status "statefulset/${resource}-restate" --timeout=180s
for attempt in $(seq 1 90); do
  if ctl status > "$run/recovered-status.txt" && \
      python3 scripts/check_loadtest_cluster.py recovery "$run/nodes.json" "$run/recovered-status.txt" "$resource-restate-2" > "$run/recovery.txt"; then break; fi
  sleep 2
done
python3 scripts/check_loadtest_cluster.py recovery "$run/nodes.json" "$run/recovered-status.txt" "$resource-restate-2"
for attempt in $(seq 1 60); do
  "${k[@]}" exec "deployment/${resource}-proxy-${generation}" -- wget -qO- "http://${resource}-metrics:9090/api/v1/targets" > "$run/metrics-targets.json"
  if python3 scripts/check_loadtest_cluster.py metrics "$run/metrics-targets.json" > "$run/metrics.txt"; then break; fi
  sleep 2
done
python3 scripts/check_loadtest_cluster.py metrics "$run/metrics-targets.json"
printf 'topology gates passed: restate_nodes=3 metadata_members=3 replication=2 public_turns=1 peer_reads=1 quorum_nodes_unavailable=1\n' | tee "$run/result.txt"
}

main "$@"; exit "$?"
