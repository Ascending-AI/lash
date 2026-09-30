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
# The rolling-upgrade campaign's rollback (FIG-3805): N returns beside both
# earlier generations without running its migrate over N+1's expansion.
"$helm_bin" template topology "$chart" -f "$chart/values-local.yaml" \
  --set workers.generation=rollback --set 'workers.retainedGenerations[0]=initial' \
  --set 'workers.retainedGenerations[1]=next' --set workers.generationImages.initial=old \
  --set workers.generationImages.next=new --set workers.generationImages.rollback=old \
  --set workers.migrate=false --set postgres.maxGenerations=3 --set postgres.maxConnections=130 \
  > target/loadtest-tools/rollback-generations.yaml
"$helm_bin" template topology "$chart" -f "$chart/values-local.yaml" \
  --set load.enabled=true > target/loadtest-tools/load-enabled.yaml
"$helm_bin" template topology "$chart" -f "$chart/values-local.yaml" \
  --set load.enabled=true --set load.faultCampaign=true --set load.run=smoke-v1-fault > target/loadtest-tools/load-campaign.yaml
printf 'chart profiles passed: lint=2 template=7\n'
