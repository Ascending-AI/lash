#!/usr/bin/env bash
set -euo pipefail
umask 077

agent_workbench_port_open() {
  local host="$1" port="$2"
  timeout 2 bash -c "echo >/dev/tcp/$host/$port" >/dev/null 2>&1
}

agent_workbench_default_port_plan() {
  local base="$1"
  printf '%s\n' "$((base + 30))" "$((base + 31))" "$((base + 32))" \
    "$((base + 33))" "$((base + 34))" "$((base + 35))"
}

agent_workbench_refuse_preexisting_resources() {
  local name port
  for name in "$restate_container" "$postgres_container"; do
    if docker container inspect "$name" >/dev/null 2>&1; then
      echo "Refusing pre-existing container name: $name" >&2
      return 73
    fi
  done
  declare -A seen_ports=()
  for port in "$admin_port" "$ingress_port" "$node_port" "$endpoint_port" \
    "$postgres_port" "$postgres_endpoint_port"; do
    if [[ ! "$port" =~ ^[0-9]+$ ]] || ((10#$port > 65535)); then
      echo "Invalid owned gate port: $port" >&2
      return 2
    fi
    if [ -n "${seen_ports[$port]:-}" ]; then
      echo "Refusing duplicate owned gate port: $port" >&2
      return 73
    fi
    seen_ports[$port]=1
    if agent_workbench_port_open 127.0.0.1 "$port"; then
      echo "Refusing pre-existing listener on owned port $port" >&2
      return 73
    fi
  done
}

agent_workbench_remove_container_id() {
  local id="$1" label="$2" remaining
  [ -n "$id" ] || return 0
  if ! docker rm -f "$id" >/dev/null 2>&1; then
    echo "$label removal failed for owned ID $id" >&2
    return 1
  fi
  if ! remaining="$(docker ps -aq --no-trunc --filter "id=$id")"; then
    echo "$label removal could not be verified because Docker is unavailable" >&2
    return 1
  fi
  if [ -n "$remaining" ]; then
    echo "$label removal could not be verified for owned ID $id" >&2
    return 1
  fi
}

agent_workbench_verify_created_container() {
  local id="$1" expected_name="$2"
  [ "$(docker inspect -f '{{.State.Running}}' "$id")" = true ]
  [ "$(docker inspect -f '{{.Name}}' "$id")" = "/$expected_name" ]
  [ "$(docker inspect -f '{{index .Config.Labels "com.lash.e2e.worktree"}}' "$id")" \
    = "$LASH_GATE_WORKTREE_SLUG" ]
}

agent_workbench_cleanup() {
  local run_status="$1" cleanup_failed=0 engine_removed=1
  local addr host port pid canonical_path temp_root retained_path marker
  set +e

  if [ -n "$restate_id" ]; then
    docker logs "$restate_id" >"$restate_log" 2>&1 || true
  fi
  if [ -n "$postgres_id" ]; then
    docker logs "$postgres_id" >"$postgres_log" 2>&1 || true
  fi
  chmod 600 "$restate_log" "$postgres_log" 2>/dev/null || true

  while IFS= read -r addr; do
    [ -n "$addr" ] || continue
    host="${addr%:*}"
    port="${addr##*:}"
    if agent_workbench_port_open "$host" "$port"; then
      printf 'owned_endpoint_closed=false addr=%s\n' "$addr" >>"$cleanup_log"
      cleanup_failed=1
      engine_removed=0
    else
      printf 'owned_endpoint_closed=true addr=%s\n' "$addr" >>"$cleanup_log"
    fi
  done < <(sort -u "$endpoint_manifest")

  while IFS= read -r pid; do
    [ -n "$pid" ] || continue
    if [ -e "/proc/$pid" ]; then
      if tr '\0' '\n' <"/proc/$pid/environ" 2>/dev/null \
        | grep -Fxq 'AGENT_WORKBENCH_RECOVERY_E2E_CHILD=1'; then
        kill -KILL "$pid" >/dev/null 2>&1 || true
        for _attempt in $(seq 1 50); do
          [ ! -e "/proc/$pid" ] && break
          sleep 0.1
        done
      fi
    fi
    if [ -e "/proc/$pid" ]; then
      printf 'owned_child_reaped=false pid=%s\n' "$pid" >>"$cleanup_log"
      cleanup_failed=1
      engine_removed=0
    else
      printf 'owned_child_reaped=true pid=%s\n' "$pid" >>"$cleanup_log"
    fi
  done < <(sort -u "$child_manifest")

  if ! agent_workbench_remove_container_id "$restate_id" Restate; then
    engine_removed=0
    cleanup_failed=1
  fi
  if ! agent_workbench_remove_container_id "$postgres_id" Postgres; then
    engine_removed=0
    cleanup_failed=1
  fi
  if [ -s "$data_manifest" ] && [ -z "$restate_id" ]; then
    engine_removed=0
    cleanup_failed=1
    echo 'retained data exists without a recorded Restate container ID' >>"$cleanup_log"
  fi
  printf 'owned_engine_removal_verified=%s\n' "$engine_removed" >>"$cleanup_log"

  if ((engine_removed == 1)); then
    temp_root="$(realpath -e "${TMPDIR:-/tmp}")"
    while IFS= read -r retained_path; do
      [ -n "$retained_path" ] || continue
      if [ ! -e "$retained_path" ]; then
        echo 'owned_data_already_removed=true' >>"$cleanup_log"
        continue
      fi
      canonical_path="$(realpath -e "$retained_path")"
      marker="$canonical_path/.agent-workbench-fixture-owner"
      if [ "$(dirname "$canonical_path")" != "$temp_root" ] \
        || [ "$(cat "$marker" 2>/dev/null)" != "$cleanup_token" ]; then
        echo 'owned_data_preimage_valid=false' >>"$cleanup_log"
        cleanup_failed=1
        continue
      fi
      echo 'owned_data_preimage_valid=true' >>"$cleanup_log"
      rm -rf -- "$canonical_path"
      if [ -e "$canonical_path" ]; then
        echo 'owned_data_removed=false' >>"$cleanup_log"
        cleanup_failed=1
      else
        echo 'owned_data_removed=true' >>"$cleanup_log"
      fi
    done < <(sort -u "$data_manifest")
  else
    echo 'owned_data_retained_engine_unverified=true' >>"$cleanup_log"
  fi

  lash_gate_cleanup
  chmod 600 "$artifact_dir"/* 2>/dev/null || true
  if ((cleanup_failed != 0)); then
    echo "agent-workbench Restate E2E cleanup failed; artifacts retained at $artifact_dir" >&2
    return 97
  fi
  if ((run_status != 0)); then
    echo "agent-workbench Restate E2E failed; artifacts retained at $artifact_dir" >&2
    return "$run_status"
  fi
  if [ "${AGENT_WORKBENCH_E2E_KEEP_ARTIFACTS:-0}" != 1 ]; then
    rm -rf -- "$artifact_dir"
  fi
}

if [ "${BASH_SOURCE[0]}" != "$0" ]; then
  return 0
fi

repo="$(cd "$(dirname "$0")/.." && pwd -P)"
cd "$repo"
source "$repo/scripts/worktree-gate-env.sh"
lash_gate_acquire agent-workbench-restate-e2e

image="${AGENT_WORKBENCH_RESTATE_IMAGE:-restatedev/restate:1.7.0}"
restate_container="${AGENT_WORKBENCH_RESTATE_CONTAINER:-lash-agent-workbench-restate-${LASH_GATE_WORKTREE_SLUG}}"
postgres_container="${AGENT_WORKBENCH_E2E_POSTGRES_CONTAINER:-lash-agent-workbench-postgres-${LASH_GATE_WORKTREE_SLUG}}"
mapfile -t default_ports < <(agent_workbench_default_port_plan "$LASH_E2E_PORT_BASE")
admin_port="${AGENT_WORKBENCH_RESTATE_ADMIN_PORT:-${default_ports[0]}}"
ingress_port="${AGENT_WORKBENCH_RESTATE_INGRESS_PORT:-${default_ports[1]}}"
node_port="${AGENT_WORKBENCH_RESTATE_NODE_PORT:-${default_ports[2]}}"
endpoint_bind="${AGENT_WORKBENCH_E2E_ENDPOINT_BIND:-127.0.0.1:${default_ports[3]}}"
postgres_port="${AGENT_WORKBENCH_E2E_POSTGRES_PORT:-${default_ports[4]}}"
postgres_endpoint_bind="${AGENT_WORKBENCH_E2E_POSTGRES_ENDPOINT_BIND:-127.0.0.1:${default_ports[5]}}"
endpoint_port="${endpoint_bind##*:}"
postgres_endpoint_port="${postgres_endpoint_bind##*:}"
database_url="${AGENT_WORKBENCH_E2E_DATABASE_URL:-postgres://lash:lash@127.0.0.1:$postgres_port/lash}"
admin_url="${RESTATE_ADMIN_URL:-http://127.0.0.1:$admin_port}"
ingress_url="${RESTATE_INGRESS_URL:-http://127.0.0.1:$ingress_port}"
if [ -n "${AGENT_WORKBENCH_E2E_ARTIFACT_DIR:-}" ]; then
  artifact_dir="$AGENT_WORKBENCH_E2E_ARTIFACT_DIR"
  mkdir -m 700 "$artifact_dir"
else
  artifact_dir="$(mktemp -d "${TMPDIR:-/tmp}/lash-agent-workbench-restate-e2e-${LASH_GATE_WORKTREE_SLUG}.XXXXXX")"
fi
build_log="$artifact_dir/build.jsonl"
test_output="$artifact_dir/test.log"
restate_log="$artifact_dir/restate.log"
postgres_log="$artifact_dir/postgres.log"
cleanup_log="$artifact_dir/cleanup.log"
data_manifest="$artifact_dir/fixture-data.manifest"
child_manifest="$artifact_dir/fixture-children.manifest"
endpoint_manifest="$artifact_dir/fixture-endpoints.manifest"
cleanup_token="$(cat /proc/sys/kernel/random/uuid)"
restate_id=""
postgres_id=""
: >"$data_manifest"
: >"$child_manifest"
: >"$endpoint_manifest"
: >"$cleanup_log"
chmod 600 "$artifact_dir"/*
export AGENT_WORKBENCH_FIXTURE_DATA_MANIFEST="$data_manifest"
export AGENT_WORKBENCH_FIXTURE_CHILD_MANIFEST="$child_manifest"
export AGENT_WORKBENCH_FIXTURE_ENDPOINT_MANIFEST="$endpoint_manifest"
export AGENT_WORKBENCH_FIXTURE_CLEANUP_TOKEN="$cleanup_token"
ulimit -c 0

cleanup_trap() {
  local run_status="$?" cleanup_status
  trap - EXIT
  agent_workbench_cleanup "$run_status"
  cleanup_status="$?"
  exit "$cleanup_status"
}
trap cleanup_trap EXIT

agent_workbench_refuse_preexisting_resources
bash "$repo/scripts/docker-pull-with-retry.sh" "$image"
bash "$repo/scripts/docker-pull-with-retry.sh" postgres:16-alpine
# Pulls can take long enough for an unrelated process to occupy a configured
# port or name. Refuse again immediately before creating either service.
agent_workbench_refuse_preexisting_resources

restate_id="$(docker run -d --name "$restate_container" --label "$LASH_GATE_LABEL" --network host \
  -e RESTATE_ADMIN__BIND_PORT="$admin_port" \
  -e RESTATE_INGRESS__BIND_PORT="$ingress_port" \
  -e RESTATE_BIND_PORT="$node_port" \
  "$image")"
agent_workbench_verify_created_container "$restate_id" "$restate_container"
postgres_id="$(docker run -d --name "$postgres_container" --label "$LASH_GATE_LABEL" --network host \
  -e POSTGRES_USER=lash -e POSTGRES_PASSWORD=lash -e POSTGRES_DB=lash \
  postgres:16-alpine -p "$postgres_port")"
agent_workbench_verify_created_container "$postgres_id" "$postgres_container"

deadline=$((SECONDS + 60))
for ready_spec in "Restate-admin:$admin_port:$restate_id" \
  "Restate-ingress:$ingress_port:$restate_id" "Postgres:$postgres_port:$postgres_id"; do
  service="${ready_spec%%:*}"
  remainder="${ready_spec#*:}"
  port="${remainder%%:*}"
  id="${remainder#*:}"
  until agent_workbench_port_open 127.0.0.1 "$port"; do
    if ((SECONDS >= deadline)); then
      docker logs "$id" >&2 || true
      echo "$service port $port did not become ready" >&2
      exit 1
    fi
    sleep 1
  done
done

set +e
cargo test --workspace --all-targets --locked --no-run \
  --message-format=json-render-diagnostics 2>&1 | tee "$build_log"
build_status="${PIPESTATUS[0]}"
set -e
if ((build_status != 0)); then
  exit "$build_status"
fi

test_executable="$(python3 - "$build_log" <<'PY'
import json
import sys

executables = []
with open(sys.argv[1], encoding="utf-8") as stream:
    for line in stream:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        target = event.get("target", {})
        if (
            event.get("reason") == "compiler-artifact"
            and target.get("name") == "agent-workbench"
            and "bin" in target.get("kind", [])
            and event.get("profile", {}).get("test")
            and event.get("executable")
        ):
            executables.append(event["executable"])
if len(executables) != 1:
    raise SystemExit(f"expected one agent-workbench test executable, found {executables!r}")
print(executables[0])
PY
)"
conformance_executable="$(python3 - "$build_log" <<'PY'
import json
import sys

executables = []
with open(sys.argv[1], encoding="utf-8") as stream:
    for line in stream:
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue
        target = event.get("target", {})
        if (
            event.get("reason") == "compiler-artifact"
            and event.get("package_id", "").endswith(
                "/crates/lash-conformance#lash-internal-conformance@0.0.0-dev"
            )
            and target.get("name") == "lash_conformance"
            and "lib" in target.get("kind", [])
            and event.get("profile", {}).get("test")
            and event.get("executable")
        ):
            executables.append(event["executable"])
if len(executables) != 1:
    raise SystemExit(f"expected one lash-conformance lib test executable, found {executables!r}")
print(executables[0])
PY
)"

set +e
RESTATE_INGRESS_URL="$ingress_url" \
RESTATE_ADMIN_URL="$admin_url" \
AGENT_WORKBENCH_E2E_ENDPOINT_BIND="$endpoint_bind" \
AGENT_WORKBENCH_E2E_POSTGRES_ENDPOINT_BIND="$postgres_endpoint_bind" \
AGENT_WORKBENCH_E2E_DATABASE_URL="$database_url" \
"$test_executable" "${AGENT_WORKBENCH_E2E_TEST_FILTER:-live_restate_}" \
  --ignored --nocapture --test-threads=1 2>&1 | tee "$test_output"
test_status="${PIPESTATUS[0]}"
set -e
if ((test_status != 0)); then
  exit "$test_status"
fi

companion_output="$artifact_dir/companion.log"
"$conformance_executable" \
  turn_input_claims_supersede_across_session_lease_generations \
  --test-threads=1 2>&1 | tee "$companion_output" | tee -a "$test_output"
if ! grep -Fq 'test result: ok. 2 passed; 0 failed' "$companion_output"; then
  echo 'companion gate: FAILED (expected two passing turn-input lease-generation tests)' >&2
  exit 1
fi
if grep -Fn 'panicked at' "$test_output" >&2; then
  echo "panic gate: FAILED ('panicked at' found in agent-workbench Restate E2E output)" >&2
  exit 1
fi
echo "panic gate: clean (no 'panicked at' lines in agent-workbench Restate E2E output)"
