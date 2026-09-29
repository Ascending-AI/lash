#!/usr/bin/env bash
set -euo pipefail
repo="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo"
bash scripts/ensure-loadtest-tools.sh
helm_bin="$repo/target/loadtest-tools/helm"
chart=deploy/helm/lash-loadtest
for profile in values-local.yaml values-scaleway.yaml; do
  "$helm_bin" lint "$chart" -f "$chart/$profile"
  "$helm_bin" template topology "$chart" -f "$chart/$profile" > "target/loadtest-tools/$profile.rendered.yaml"
done
"$helm_bin" template topology "$chart" -f "$chart/values-scaleway.yaml" \
  --set s3.mode=external --set s3.externalEndpoint=https://s3.fr-par.scw.cloud --set s3.region=fr-par > target/loadtest-tools/scaleway-object-storage.yaml
"$helm_bin" template topology "$chart" -f "$chart/values-local.yaml" \
  --set workers.generation=next --set 'workers.retainedGenerations[0]=initial' \
  --set workers.generationImages.initial=old --set workers.generationImages.next=new > target/loadtest-tools/retained-generations.yaml
python3 scripts/test_loadtest_topology.py "$@"
printf 'chart profiles passed: lint=2 template=4\n'
