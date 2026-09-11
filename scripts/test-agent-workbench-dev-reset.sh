#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
test_tmp="$(mktemp -d)"
mock_bin="$test_tmp/bin"
mock_state="$test_tmp/mock-state"
mkdir -p "$mock_bin" "$mock_state" "$test_tmp/runtime"
launcher_runtime_root="/tmp/lash-agent-workbench-$UID"

cleanup_fixture_runtime_resources() {
  local runtime_root="$1" fixture_root="$2" fixture_state="$3"
  local file id token component lease
  [[ -d "$runtime_root" ]] || return 0
  for file in "$fixture_state"/container-*; do
    [[ -f "$file" && "$file" != *container-counter && "$file" != *container-ports-* ]] \
      || continue
    read -r id token component < "$file" || continue
    for lease in "$runtime_root"/"$component"-*.lease; do
      [[ -f "$lease" && ! -L "$lease" ]] || continue
      if [[ "$(<"$lease")" = "1 $component $token $id" ]]; then
        rm -f "$lease"
      fi
    done
  done
  for file in "$runtime_root"/*-recover.sh; do
    [[ -f "$file" && ! -L "$file" ]] || continue
    grep -Fq "$fixture_root" "$file" && rm -f "$file"
  done
}

cleanup() {
  local file pid start current
  while IFS= read -r file; do
    read -r pid start < "$file" || continue
    current="$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)"
    if [[ "$current" = "$start" ]]; then
      kill -- "-$pid" >/dev/null 2>&1 || kill "$pid" >/dev/null 2>&1 || true
    fi
  done < <(find "$test_tmp" -type f -name 'workbench-*.pid' -print 2>/dev/null)
  cleanup_fixture_runtime_resources "$launcher_runtime_root" "$test_tmp" "$mock_state"
  rm -rf -- "$test_tmp"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

synthetic_runtime="$test_tmp/synthetic-runtime"
mkdir -m 700 "$synthetic_runtime"
synthetic_lock="$synthetic_runtime/unrelated.lock"
exec 8> "$synthetic_lock"
flock 8
cleanup_fixture_runtime_resources "$synthetic_runtime" "$test_tmp/fixture" "$mock_state"
exec 9> "$synthetic_lock"
if flock -n 9; then
  fail "fixture cleanup unlinked an unrelated held stable lock"
fi
exec 9>&-
flock -u 8
exec 8>&-

cat > "$mock_bin/timeout" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
command_text="$*"
port="${command_text##*/}"
case " ${MOCK_EXTERNAL_PORTS:-} " in
  *" $port "*) exit 0 ;;
esac
if [[ -f "$MOCK_STATE/tcp-$port" ]]; then
  exit 0
fi
if [[ -f "$MOCK_PID_FILE" ]]; then
  read -r pid start < "$MOCK_PID_FILE" || exit 1
  current="$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)"
  [[ "$current" = "$start" ]] && exit 0
fi
exit 1
MOCK

cat > "$mock_bin/docker" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
command="$1"
shift
case "$command" in
  run)
    name="" token="" component=""
    ports=()
    args=("$@")
    for ((i = 0; i < ${#args[@]}; i++)); do
      case "${args[$i]}" in
        --name) name="${args[$((i + 1))]}" ;;
        --label)
          label="${args[$((i + 1))]}"
          case "$label" in
            com.lash.agent-workbench.owner=*) token="${label#*=}" ;;
            com.lash.agent-workbench.component=*) component="${label#*=}" ;;
          esac
          ;;
        -e)
          setting="${args[$((i + 1))]}"
          case "$setting" in
            RESTATE_INGRESS__BIND_PORT=*|RESTATE_ADMIN__BIND_PORT=*)
              ports+=("${setting#*=}")
              ;;
          esac
          ;;
        -p) ports+=("${args[$((i + 1))]}") ;;
      esac
    done
    [[ -n "$name" && -n "$token" && -n "$component" ]]
    counter=0
    [[ ! -f "$MOCK_STATE/container-counter" ]] || counter="$(<"$MOCK_STATE/container-counter")"
    counter=$((counter + 1))
    printf '%s\n' "$counter" > "$MOCK_STATE/container-counter"
    printf -v id '%064x' "$counter"
    printf '%s %s %s\n' "$id" "$token" "$component" > "$MOCK_STATE/container-$name"
    printf '%s\n' "${ports[@]}" > "$MOCK_STATE/container-ports-$name"
    for port in "${ports[@]}"; do
      : > "$MOCK_STATE/tcp-$port"
    done
    if [[ "$component" = restate && "${MOCK_BLOCK_RESTATE_MARKER:-0}" = 1 ]]; then
      mkdir -p "$MOCK_RESTATE_MARKER"
    elif [[ "$component" = postgres && "${MOCK_BLOCK_POSTGRES_MARKER:-0}" = 1 ]]; then
      mkdir -p "$MOCK_POSTGRES_MARKER"
    fi
    printf '%s\n' "$id"
    ;;
  inspect)
    format="" name=""
    while (($#)); do
      case "$1" in
        --format) format="$2"; shift 2 ;;
        *) name="$1"; shift ;;
      esac
    done
    file="$MOCK_STATE/container-$name"
    [[ -f "$file" ]] || exit 1
    if [[ -n "$format" ]]; then
      read -r id token component < "$file"
      printf '%s %s %s\n' "$id" "$token" "$component"
    fi
    ;;
  rm)
    id="${@: -1}"
    found=""
    for file in "$MOCK_STATE"/container-*; do
      [[ -f "$file" && "$file" != *container-counter && "$file" != *container-ports-* ]] || continue
      read -r actual_id token component < "$file"
      if [[ "$actual_id" = "$id" ]]; then
        printf 'rm %s %s\n' "$id" "$component" >> "$MOCK_STATE/docker-rm-attempt.log"
        if [[ "${MOCK_RM_FAIL_COMPONENT:-}" = "$component" \
          && "$id" != "${MOCK_RM_ALLOW_ID:-}" ]]; then
          failure_marker="$MOCK_STATE/docker-rm-failed-$component"
          if [[ "${MOCK_RM_FAIL_MODE:-always}" = always || ! -e "$failure_marker" ]]; then
            : > "$failure_marker"
            exit 1
          fi
        fi
        found="$file"
        printf 'rm %s %s\n' "$id" "$component" >> "$MOCK_STATE/docker-rm.log"
        rm -f "$file" "$MOCK_STATE/journal-$id"
        name="${file##*/container-}"
        service_port=""
        while IFS= read -r port; do
          service_port="$port"
          [[ -n "$port" ]] && rm -f "$MOCK_STATE/tcp-$port"
        done < "$MOCK_STATE/container-ports-$name"
        rm -f "$MOCK_STATE/container-ports-$name"
        if [[ "$component" = restate ]]; then
          awk -F '\t' -v port="$service_port" '$1 != port' "$MOCK_STATE/deployments" \
            > "$MOCK_STATE/deployments.next"
          mv "$MOCK_STATE/deployments.next" "$MOCK_STATE/deployments"
        fi
        break
      fi
    done
    [[ -n "$found" ]] || exit 1
    ;;
  logs) ;;
  *) exit 2 ;;
esac
MOCK

cat > "$mock_bin/cargo" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
count=0
[[ ! -f "$MOCK_STATE/build-count" ]] || count="$(<"$MOCK_STATE/build-count")"
printf '%s\n' "$((count + 1))" > "$MOCK_STATE/build-count"
mkdir -p "$CARGO_TARGET_DIR/judged"
cat > "$CARGO_TARGET_DIR/judged/agent-workbench" <<'BIN'
#!/usr/bin/env bash
trap 'exit 0' TERM INT
start="$(awk '{print $22}' "/proc/$$/stat")"
printf '%s %s\n' "$$" "$start" > "$MOCK_STATE/spawned-${AGENT_WORKBENCH_ADDR##*:}"
mkdir -p "$AGENT_WORKBENCH_DATA_DIR"
printf 'attempt application state\n' > "$AGENT_WORKBENCH_DATA_DIR/attempt-app-state"
{
  printf 'AGENT_WORKBENCH_ADDR=%s\n' "$AGENT_WORKBENCH_ADDR"
  printf 'AGENT_WORKBENCH_RESTATE_ADDR=%s\n' "$AGENT_WORKBENCH_RESTATE_ADDR"
  printf 'AGENT_WORKBENCH_DATABASE_URL=%s\n' "$AGENT_WORKBENCH_DATABASE_URL"
  printf 'RESTATE_INGRESS_URL=%s\n' "$RESTATE_INGRESS_URL"
  printf 'RESTATE_ADMIN_URL=%s\n' "$RESTATE_ADMIN_URL"
  printf 'AGENT_WORKBENCH_DATA_DIR=%s\n' "$AGENT_WORKBENCH_DATA_DIR"
} > "$MOCK_STATE/workbench-env-${AGENT_WORKBENCH_ADDR##*:}"
while :; do sleep 1; done
BIN
chmod +x "$CARGO_TARGET_DIR/judged/agent-workbench"
if [[ "${MOCK_BLOCK_PID_PUBLICATION:-0}" = 1 ]]; then
  mkdir -p "$MOCK_PID_FILE"
fi
if [[ " $* " = *' run '* ]]; then
  exec "$CARGO_TARGET_DIR/judged/agent-workbench"
fi
MOCK

cat > "$mock_bin/curl" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
url="${@: -1}"
payload=""
args=("$@")
for ((i = 0; i < ${#args[@]}; i++)); do
  if [[ "${args[$i]}" = --data ]]; then
    payload="${args[$((i + 1))]}"
  fi
done
if [[ "$url" = */healthz ]]; then
  [[ -f "$MOCK_PID_FILE" ]] || exit 1
  read -r pid start < "$MOCK_PID_FILE" || exit 1
  current="$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)"
  [[ "$current" = "$start" ]] || exit 1
  printf '{"service":"agent-workbench"}\n'
elif [[ "$url" = */deployments && -z "$payload" ]]; then
  admin_address="${url%/deployments}"
  admin_address="${admin_address%/}"
  admin_address="${admin_address#*://}"
  admin_address="${admin_address%%/*}"
  admin_port="${admin_address##*:}"
  printf '{"deployments":['
  separator=""
  while IFS=$'\t' read -r record_port deployment_id uri; do
    [[ "$record_port" = "$admin_port" && -n "$deployment_id" && -n "$uri" ]] || continue
    printf '%s{"id":"%s","uri":"%s"}' "$separator" "$deployment_id" "$uri"
    separator=,
  done < "$MOCK_STATE/deployments"
  printf ']}\n'
elif [[ "$url" = */deployments ]]; then
  printf '%s\n' "$payload" >> "$MOCK_STATE/registration-payloads"
  uri="$(printf '%s' "$payload" | python3 -c 'import json,sys; print(json.load(sys.stdin)["uri"])')"
  deployment_number="$(( $(wc -l < "$MOCK_STATE/registration-payloads") ))"
  deployment_id="dp_mock$deployment_number"
  admin_address="${url%/deployments}"
  admin_address="${admin_address%/}"
  admin_address="${admin_address#*://}"
  admin_address="${admin_address%%/*}"
  admin_port="${admin_address##*:}"
  printf '%s\t%s\t%s\n' "$admin_port" "$deployment_id" "$uri" >> "$MOCK_STATE/deployments"
  if [[ "${MOCK_POST_REMOVE_RESTATE_MARKER:-0}" = 1 ]]; then
    rm -f "$MOCK_RESTATE_MARKER"
  fi
  if [[ "${MOCK_POST_REMOVE_PID:-0}" = 1 && -f "$MOCK_PID_FILE" ]]; then
    cp "$MOCK_PID_FILE" "$MOCK_STATE/removed-pid-record"
    rm -f "$MOCK_PID_FILE" "${MOCK_PID_FILE%.pid}.meta"
  elif [[ "${MOCK_POST_KILL:-0}" = 1 && -f "$MOCK_PID_FILE" ]]; then
    read -r pid _ < "$MOCK_PID_FILE"
    kill -- "-$pid" >/dev/null 2>&1 || kill "$pid" >/dev/null 2>&1 || true
    for _ in {1..100}; do
      kill -0 "$pid" >/dev/null 2>&1 || break
      sleep 0.01
    done
  fi
  printf '{"id":"%s"}\n' "$deployment_id"
else
  exit 2
fi
MOCK

chmod +x "$mock_bin"/*
: > "$mock_state/deployments"
: > "$mock_state/registration-payloads"
: > "$mock_state/docker-rm.log"
: > "$mock_state/docker-rm-attempt.log"

launcher_env() {
  local data_dir="$1" port="$2"
  shift 2
  env PATH="$mock_bin:$PATH" \
    MOCK_STATE="$mock_state" \
    MOCK_PID_FILE="$data_dir/run/workbench-127.0.0.1_${port}.pid" \
    MOCK_RESTATE_MARKER="$data_dir/run/restate-127.0.0.1_${port}.container" \
    MOCK_POSTGRES_MARKER="$data_dir/run/postgres-127.0.0.1_${port}.container" \
    XDG_RUNTIME_DIR="$test_tmp/runtime" \
    CARGO_TARGET_DIR="$test_tmp/target-$port" \
    AGENT_WORKBENCH_RUN_DIR="$data_dir/run" \
    AGENT_WORKBENCH_DATA_DIR="$data_dir" \
    RESTATE_ADMIN_URL="http://127.0.0.1:$((19070 + (port - 3030) * 10))/v2" \
    AGENT_WORKBENCH_OPEN=0 \
    AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion \
    "$@"
}

run_launcher() {
  local data_dir="$1" port="$2"
  shift 2
  launcher_env "$data_dir" "$port" \
    bash "$repo_root/scripts/agent-workbench-dev.sh" "$@" --port "$port"
}

pid_identity() {
  local file="$1"
  read -r pid start < "$file"
  current="$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)"
  [[ "$current" = "$start" ]]
}

assert_count() {
  local expected="$1" file="$2"
  [[ "$(wc -l < "$file")" = "$expected" ]] \
    || fail "expected $expected lines in $file"
}

data_sqlite="$test_tmp/data-sqlite"
port_sqlite=3032
run_launcher "$data_sqlite" "$port_sqlite" up > "$test_tmp/up.log" 2>&1
pid_file="$data_sqlite/run/workbench-127.0.0.1_${port_sqlite}.pid"
reset_file="$data_sqlite/run/reset-127.0.0.1_${port_sqlite}.meta"
[[ -f "$reset_file" && -f "$data_sqlite/.agent-workbench-dev-reset-owner" ]] \
  || fail "fresh owned SQLite stack did not record reset ownership"
grep -Eq '^owned_restate_deployment_id=dp_mock[0-9]+$' "$reset_file" \
  || fail "fresh owned SQLite stack did not record its Restate deployment id"
grep -Eq '^owned_restate_registry_hash=[0-9a-f]{64}$' "$reset_file" \
  || fail "fresh owned SQLite stack did not record its exact Restate registry snapshot"
pid_identity "$pid_file" || fail "fresh SQLite workbench is not alive"
[[ "$(<"$mock_state/registration-payloads")" = '{"uri":"http://127.0.0.1:9101","force":false,"breaking":false}' ]] \
  || fail "fresh registration did not disable replacement on a v2 admin URL"

fresh_pid_record="$(<"$pid_file")"
fresh_build_count="$(<"$mock_state/build-count")"
run_launcher "$data_sqlite" "$port_sqlite" up > "$test_tmp/up-idempotent.log" 2>&1
[[ "$(<"$pid_file")" = "$fresh_pid_record" && "$(<"$mock_state/build-count")" = "$fresh_build_count" ]] \
  || fail "live up was not idempotent"
assert_count 1 "$mock_state/registration-payloads"

printf 'durable application state\n' > "$data_sqlite/durable-seed"
old_pid_record="$(<"$pid_file")"
old_restate_record="$(<"$data_sqlite/run/restate-127.0.0.1_${port_sqlite}.container")"
old_restate_id="${old_restate_record#* }"
old_restate_id="${old_restate_id%% *}"
printf 'durable Restate journal\n' > "$mock_state/journal-$old_restate_id"
builds_before="$(<"$mock_state/build-count")"
if run_launcher "$data_sqlite" "$port_sqlite" restart > "$test_tmp/restart-refusal.log" 2>&1; then
  fail "ordinary restart unexpectedly succeeded"
fi
grep -Fq 'restart --reset-dev-state' "$test_tmp/restart-refusal.log" \
  || fail "ordinary restart refusal omitted the explicit reset command"
[[ "$(<"$pid_file")" = "$old_pid_record" ]] && pid_identity "$pid_file" \
  || fail "ordinary restart changed the live PID"
[[ -f "$data_sqlite/durable-seed" && -f "$mock_state/journal-$old_restate_id" ]] \
  || fail "ordinary restart mutated durable state"
[[ "$(<"$mock_state/build-count")" = "$builds_before" ]] \
  || fail "ordinary restart built before refusing"
assert_count 1 "$mock_state/registration-payloads"
assert_count 0 "$mock_state/docker-rm.log"

run_launcher "$data_sqlite" "$port_sqlite" restart --reset-dev-state \
  > "$test_tmp/reset-success.log" 2>&1
[[ ! -e "$data_sqlite/durable-seed" && ! -e "$mock_state/journal-$old_restate_id" ]] \
  || fail "explicit reset retained old SQLite or Restate state"
pid_identity "$pid_file" || fail "replacement SQLite workbench is not alive"
[[ "$(<"$pid_file")" != "$old_pid_record" ]] || fail "reset reused the old process identity"
assert_count 2 "$mock_state/registration-payloads"
if grep -Eq '"force":true|"breaking":true' "$mock_state/registration-payloads"; then
  fail "registration payload enabled force or breaking"
fi
grep -Fq "rm $old_restate_id restate" "$mock_state/docker-rm.log" \
  || fail "reset did not remove the exact old Restate container"

stable_pid_record="$(<"$pid_file")"
mutation_count="$(wc -l < "$mock_state/docker-rm.log")"
if launcher_env "$data_sqlite" "$port_sqlite" \
  AGENT_WORKBENCH_DATABASE_URL='postgres://secret@example.invalid/lash' \
  bash "$repo_root/scripts/agent-workbench-dev.sh" restart --reset-dev-state --port "$port_sqlite" \
  > "$test_tmp/external-db-refusal.log" 2>&1; then
  fail "mixed external database reset unexpectedly succeeded"
fi
[[ "$(<"$pid_file")" = "$stable_pid_record" ]] && pid_identity "$pid_file" \
  || fail "external database refusal changed the live stack"
[[ "$(wc -l < "$mock_state/docker-rm.log")" = "$mutation_count" ]] \
  || fail "external database refusal removed a container"
! grep -Fq 'postgres://secret' "$test_tmp/external-db-refusal.log" \
  || fail "external database refusal exposed credentials"

read -r stable_pid stable_start <<<"$stable_pid_record"
printf '%s %s\n' "$stable_pid" "$((stable_start + 1))" > "$pid_file"
if run_launcher "$data_sqlite" "$port_sqlite" restart --reset-dev-state \
  > "$test_tmp/pid-refusal.log" 2>&1; then
  fail "mismatched PID reset unexpectedly succeeded"
fi
[[ "$(wc -l < "$mock_state/docker-rm.log")" = "$mutation_count" ]] \
  || fail "mismatched PID refusal removed a container"
printf '%s\n' "$stable_pid_record" > "$pid_file"
pid_identity "$pid_file" || fail "mismatched PID refusal signaled the unrelated identity"

restate_marker="$data_sqlite/run/restate-127.0.0.1_${port_sqlite}.container"
owned_restate_marker="$(<"$restate_marker")"
printf '%s\n' "${owned_restate_marker%% *}" > "$restate_marker"
if run_launcher "$data_sqlite" "$port_sqlite" restart --reset-dev-state \
  > "$test_tmp/legacy-marker-refusal.log" 2>&1; then
  fail "legacy container marker reset unexpectedly succeeded"
fi
[[ "$(<"$pid_file")" = "$stable_pid_record" ]] && pid_identity "$pid_file" \
  || fail "legacy marker refusal changed the live stack"
[[ "$(wc -l < "$mock_state/docker-rm.log")" = "$mutation_count" ]] \
  || fail "legacy marker refusal removed a container"
printf '%s\n' "$owned_restate_marker" > "$restate_marker"
chmod 600 "$restate_marker"

if launcher_env "$data_sqlite" "$port_sqlite" \
  AGENT_WORKBENCH_RESTATE_CONTAINER=changed-container-name \
  bash "$repo_root/scripts/agent-workbench-dev.sh" restart --reset-dev-state --port "$port_sqlite" \
  > "$test_tmp/container-setting-refusal.log" 2>&1; then
  fail "changed container setting reset unexpectedly succeeded"
fi
[[ "$(<"$pid_file")" = "$stable_pid_record" ]] && pid_identity "$pid_file" \
  || fail "changed container setting refusal changed the live stack"
[[ "$(wc -l < "$mock_state/docker-rm.log")" = "$mutation_count" ]] \
  || fail "changed container setting refusal removed a container"

ln -s "$data_sqlite" "$test_tmp/data-symlink"
if launcher_env "$test_tmp/data-symlink" "$port_sqlite" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" restart --reset-dev-state --port "$port_sqlite" \
  > "$test_tmp/symlink-refusal.log" 2>&1; then
  fail "symlinked data reset unexpectedly succeeded"
fi
[[ "$(<"$pid_file")" = "$stable_pid_record" ]] && pid_identity "$pid_file" \
  || fail "symlink refusal changed the live stack"

lock_hash="$(printf '%s' "$repo_root" | sha256sum | awk '{print $1}')"
lock_file="$launcher_runtime_root/$lock_hash.lock"
exec 9> "$lock_file"
flock 9
if run_launcher "$data_sqlite" "$port_sqlite" down \
  > "$test_tmp/concurrent-refusal.log" 2>&1; then
  fail "competing lifecycle command ignored the launcher lock"
fi
flock -u 9
exec 9>&-
grep -Fq 'another launcher lifecycle command' "$test_tmp/concurrent-refusal.log" \
  || fail "competing lifecycle refusal was not precise"
[[ "$(<"$pid_file")" = "$stable_pid_record" ]] && pid_identity "$pid_file" \
  || fail "competing lifecycle command changed the live stack"

printf 'second disposable state\n' > "$data_sqlite/durable-seed"
if launcher_env "$data_sqlite" "$port_sqlite" MOCK_POST_KILL=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" restart --reset-dev-state --port "$port_sqlite" \
  > "$test_tmp/reset-failure.log" 2>&1; then
  fail "replacement registration failure unexpectedly succeeded"
fi
[[ ! -e "$data_sqlite/durable-seed" ]] \
  || fail "failed replacement claimed reset without clearing old application state"
[[ ! -e "$pid_file" ]] || fail "failed replacement left a managed PID"
[[ ! -e "$data_sqlite/run/restate-127.0.0.1_${port_sqlite}.container" ]] \
  || fail "failed replacement left its Restate ownership marker"
[[ ! -e "$mock_state/container-lash-agent-workbench-dev-restate-$port_sqlite" ]] \
  || fail "failed replacement left its Restate container"
grep -Fq 'the disposable dev state was reset, but replacement startup failed' \
  "$test_tmp/reset-failure.log" || fail "failed replacement omitted reset status"
grep -Fq 'recovery command saved at ' "$test_tmp/reset-failure.log" \
  || fail "failed replacement omitted recovery command location"
recovery_file="$(sed -n 's/^\[agent-workbench\] stack is stopped; recovery command saved at //p' "$test_tmp/reset-failure.log")"
[[ -f "$recovery_file" && ! -L "$recovery_file" ]] \
  || fail "failed replacement did not save the recovery command"
[[ "$(stat -c '%a' "$recovery_file")" = 600 ]] \
  || fail "recovery command is not private"
grep -Fq 'agent-workbench-dev.sh up --addr 127.0.0.1:3032' "$recovery_file" \
  || fail "saved recovery command does not target the reset stack"
env PATH="$mock_bin:$PATH" \
  MOCK_STATE="$mock_state" \
  MOCK_PID_FILE="$pid_file" \
  XDG_RUNTIME_DIR="$test_tmp/runtime" \
  CARGO_TARGET_DIR="$test_tmp/target-$port_sqlite" \
  AGENT_WORKBENCH_RUN_DIR="$test_tmp/wrong-run" \
  AGENT_WORKBENCH_DATA_DIR="$test_tmp/wrong-data" \
  AGENT_WORKBENCH_RESTATE_ADDR=127.0.0.1:65501 \
  AGENT_WORKBENCH_RESTATE_ENDPOINT_URL=http://127.0.0.1:65501 \
  RESTATE_INGRESS_URL=http://127.0.0.1:65502 \
  RESTATE_ADMIN_URL=http://127.0.0.1:65503/v1 \
  AGENT_WORKBENCH_RESTATE_NODE_PORT=65504 \
  AGENT_WORKBENCH_RESTATE_CONTAINER=wrong-restate-container \
  AGENT_WORKBENCH_DATABASE_URL=postgres://wrong.invalid/wrong \
  AGENT_WORKBENCH_POSTGRES=1 \
  AGENT_WORKBENCH_OPEN=0 \
  AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion \
  bash "$recovery_file" \
  > "$test_tmp/recovery-success.log" 2>&1
pid_identity "$pid_file" || fail "saved recovery script did not start a healthy replacement"
assert_count 4 "$mock_state/registration-payloads"
[[ ! -e "$recovery_file" ]] || fail "successful recovery retained a stale recovery script"
expected_recovery_env="$test_tmp/expected-recovery-env"
cat > "$expected_recovery_env" <<EOF
AGENT_WORKBENCH_ADDR=127.0.0.1:3032
AGENT_WORKBENCH_RESTATE_ADDR=127.0.0.1:9101
AGENT_WORKBENCH_DATABASE_URL=
RESTATE_INGRESS_URL=http://127.0.0.1:8100
RESTATE_ADMIN_URL=http://127.0.0.1:19090/v2
AGENT_WORKBENCH_DATA_DIR=$data_sqlite
EOF
cmp -s "$expected_recovery_env" "$mock_state/workbench-env-$port_sqlite" \
  || fail "saved recovery script did not transmit the original stack settings"
[[ -f "$mock_state/container-lash-agent-workbench-dev-restate-$port_sqlite" \
  && ! -e "$mock_state/container-wrong-restate-container" ]] \
  || fail "saved recovery script did not restore the original container identity"

data_postgres="$test_tmp/data-postgres"
port_postgres=3034
launcher_env "$data_postgres" "$port_postgres" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_postgres" \
  > "$test_tmp/postgres-up.log" 2>&1
postgres_pid_file="$data_postgres/run/workbench-127.0.0.1_${port_postgres}.pid"
pid_identity "$postgres_pid_file" || fail "fresh managed-Postgres workbench is not alive"
postgres_marker="$data_postgres/run/postgres-127.0.0.1_${port_postgres}.container"
read -r _ old_postgres_id _ _ < "$postgres_marker"
printf 'managed Postgres state\n' > "$mock_state/journal-$old_postgres_id"
launcher_env "$data_postgres" "$port_postgres" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" restart --reset-dev-state --port "$port_postgres" \
  > "$test_tmp/postgres-reset.log" 2>&1
grep -Fq "rm $old_postgres_id postgres" "$mock_state/docker-rm.log" \
  || fail "managed Postgres reset did not remove the exact old container"
[[ ! -e "$mock_state/journal-$old_postgres_id" ]] \
  || fail "managed Postgres reset retained old container state"
pid_identity "$postgres_pid_file" || fail "replacement managed-Postgres workbench is not alive"

data_existing="$test_tmp/data-existing-uri"
port_existing=3036
printf '19130\tdp_existing\thttp://127.0.0.1:9141/\n' > "$mock_state/deployments"
builds_before="$(<"$mock_state/build-count")"
if launcher_env "$data_existing" "$port_existing" MOCK_EXTERNAL_PORTS='8140 19130' \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_existing" \
  > "$test_tmp/existing-uri-refusal.log" 2>&1; then
  fail "fresh up replaced an already-registered URI"
fi
[[ "$(<"$mock_state/build-count")" = "$builds_before" ]] \
  || fail "existing-URI refusal built a new host"
grep -Fq 'already registered' "$test_tmp/existing-uri-refusal.log" \
  || fail "existing-URI refusal was not precise"

data_unowned="$test_tmp/data-unowned-name"
port_unowned=3038
unowned_name="lash-agent-workbench-dev-restate-$port_unowned"
unowned_id="ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
printf '%s %s restate\n' "$unowned_id" '00000000-0000-0000-0000-000000000000' \
  > "$mock_state/container-$unowned_name"
rm -f "$mock_state/deployments"
: > "$mock_state/deployments"
rm_count_before="$(wc -l < "$mock_state/docker-rm.log")"
if launcher_env "$data_unowned" "$port_unowned" MOCK_UNREADY_CONTAINER_NAME="$unowned_name" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_unowned" \
  > "$test_tmp/unowned-name-refusal.log" 2>&1; then
  fail "launcher adopted an unowned same-name Restate container"
fi
[[ -f "$mock_state/container-$unowned_name" ]] \
  || fail "launcher deleted an unowned same-name container"
[[ "$(wc -l < "$mock_state/docker-rm.log")" = "$rm_count_before" ]] \
  || fail "unowned same-name refusal removed a container"

if launcher_env "$test_tmp/data-secret-url" 3040 \
  RESTATE_ADMIN_URL='http://user:diagnostic-secret@127.0.0.1:19170/v2' \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3040 \
  > "$test_tmp/secret-url-refusal.log" 2>&1; then
  fail "credential-bearing diagnostic URL unexpectedly succeeded"
fi
! grep -Fq 'diagnostic-secret' "$test_tmp/secret-url-refusal.log" \
  || fail "credential-bearing URL was exposed in diagnostics"

nonloopback_builds_before="$(<"$mock_state/build-count")"
if launcher_env "$test_tmp/data-nonloopback-service" 3040 \
  RESTATE_INGRESS_URL=http://192.0.2.10:8180 \
  RESTATE_ADMIN_URL=http://192.0.2.10:19170/v2 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3040 \
  > "$test_tmp/nonloopback-service-refusal.log" 2>&1; then
  fail "ambiguous non-loopback service identity unexpectedly succeeded"
fi
[[ ! -e "$test_tmp/data-nonloopback-service" \
  && "$(<"$mock_state/build-count")" = "$nonloopback_builds_before" ]] \
  || fail "non-loopback service refusal mutated candidate state"
grep -Fq 'service host must be a numeric loopback address or localhost' \
  "$test_tmp/nonloopback-service-refusal.log" \
  || fail "non-loopback service refusal did not state the supported identity boundary"

data_missing_pid="$test_tmp/data-missing-pid"
port_missing_pid=3042
if launcher_env "$data_missing_pid" "$port_missing_pid" MOCK_POST_REMOVE_PID=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_missing_pid" \
  > "$test_tmp/missing-pid-cleanup.log" 2>&1; then
  fail "post-registration PID metadata loss unexpectedly succeeded"
fi
read -r removed_pid removed_start < "$mock_state/removed-pid-record"
removed_current="$(awk '{print $22}' "/proc/$removed_pid/stat" 2>/dev/null || true)"
[[ "$removed_current" != "$removed_start" ]] \
  || fail "cleanup lost process metadata and left the owned workbench running"
grep -Fq "stopping process $removed_pid" "$test_tmp/missing-pid-cleanup.log" \
  || fail "cleanup did not use its retained exact process identity"
[[ ! -e "$data_missing_pid" ]] \
  || fail "successful exact process and engine retirement retained attempt application state"
[[ ! -e "$mock_state/container-lash-agent-workbench-dev-restate-$port_missing_pid" ]] \
  || fail "successful cleanup after PID metadata loss retained Restate"

data_failed_process="$test_tmp/data-failed-process"
port_failed_process=3050
process_rm_attempts_before="$(wc -l < "$mock_state/docker-rm-attempt.log")"
if launcher_env "$data_failed_process" "$port_failed_process" \
  MOCK_POST_REMOVE_PID=1 'BASH_FUNC_kill%%=() { return 1; }' \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_failed_process" \
  > "$test_tmp/failed-process-retirement.log" 2>&1; then
  fail "startup with persistently failed process retirement unexpectedly succeeded"
fi
read -r failed_process_pid failed_process_start < "$mock_state/removed-pid-record"
failed_process_current="$(awk '{print $22}' "/proc/$failed_process_pid/stat" 2>/dev/null || true)"
[[ "$failed_process_current" = "$failed_process_start" \
  && -f "$data_failed_process/attempt-app-state" \
  && -f "$data_failed_process/.agent-workbench-dev-reset-owner" \
  && -f "$data_failed_process/run/reset-127.0.0.1_${port_failed_process}.meta" ]] \
  || fail "failed process retirement did not retain its live process and application state"
[[ "$(wc -l < "$mock_state/docker-rm-attempt.log")" = "$process_rm_attempts_before" ]] \
  || fail "failed process retirement attempted to remove its Restate engine"
grep -Fq 'startup cleanup could not stop the owned workbench' \
  "$test_tmp/failed-process-retirement.log" \
  || fail "failed process retirement did not report its retained state"
kill -- "-$failed_process_pid" >/dev/null 2>&1 \
  || kill "$failed_process_pid" >/dev/null 2>&1 \
  || fail "test could not stop its retained mock workbench"
for _ in {1..100}; do
  failed_process_current="$(awk '{print $22}' "/proc/$failed_process_pid/stat" 2>/dev/null || true)"
  [[ "$failed_process_current" != "$failed_process_start" ]] && break
  sleep 0.01
done
[[ "$failed_process_current" != "$failed_process_start" ]] \
  || fail "retained mock workbench did not stop during test cleanup"

data_failed_retirement="$test_tmp/data-failed-retirement"
port_failed_retirement=3044
rm_attempts_before_failure="$(wc -l < "$mock_state/docker-rm-attempt.log")"
if launcher_env "$data_failed_retirement" "$port_failed_retirement" \
  AGENT_WORKBENCH_POSTGRES=1 MOCK_POST_KILL=1 \
  MOCK_RM_FAIL_COMPONENT=restate MOCK_RM_FAIL_MODE=always \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_failed_retirement" \
  > "$test_tmp/failed-retirement.log" 2>&1; then
  fail "startup with persistently failed Restate retirement unexpectedly succeeded"
fi
failed_state_key="127.0.0.1_${port_failed_retirement}"
[[ -f "$data_failed_retirement/attempt-app-state" \
  && -f "$data_failed_retirement/.agent-workbench-dev-reset-owner" \
  && -f "$data_failed_retirement/run/reset-$failed_state_key.meta" ]] \
  || fail "failed Restate retirement deleted application state or ownership metadata"
[[ -f "$data_failed_retirement/run/restate-$failed_state_key.container" \
  && -f "$data_failed_retirement/run/postgres-$failed_state_key.container" \
  && -f "$mock_state/container-lash-agent-workbench-dev-restate-$port_failed_retirement" \
  && -f "$mock_state/container-lash-agent-workbench-dev-postgres-$port_failed_retirement" ]] \
  || fail "failed Restate retirement discarded exact engine or store ownership evidence"
grep -Fq 'startup cleanup is incomplete; retained its application state and ownership metadata' \
  "$test_tmp/failed-retirement.log" \
  || fail "failed Restate retirement did not report retained partial state"
read -r _ failed_restate_id _ _ \
  < "$data_failed_retirement/run/restate-$failed_state_key.container"
read -r _ failed_postgres_id _ _ \
  < "$data_failed_retirement/run/postgres-$failed_state_key.container"
mapfile -t failed_retirement_attempts \
  < <(tail -n "+$((rm_attempts_before_failure + 1))" "$mock_state/docker-rm-attempt.log")
[[ "${#failed_retirement_attempts[@]}" = 2 \
  && "${failed_retirement_attempts[0]}" = "rm $failed_restate_id restate" \
  && "${failed_retirement_attempts[1]}" = "rm $failed_restate_id restate" ]] \
  || fail "failed Restate retirement did not make the bounded exact-ID retry"
! grep -Fq "rm $failed_postgres_id postgres" "$mock_state/docker-rm.log" \
  || fail "failed Restate retirement removed its dependent Postgres store"

data_retry_cleanup="$test_tmp/data-retry-cleanup"
port_retry_cleanup=3046
rm -f "$mock_state/docker-rm-failed-restate"
rm_log_before_retry="$(wc -l < "$mock_state/docker-rm.log")"
if launcher_env "$data_retry_cleanup" "$port_retry_cleanup" \
  AGENT_WORKBENCH_POSTGRES=1 MOCK_POST_KILL=1 \
  MOCK_RM_FAIL_COMPONENT=restate MOCK_RM_FAIL_MODE=once \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_retry_cleanup" \
  > "$test_tmp/retry-cleanup.log" 2>&1; then
  fail "post-registration process failure unexpectedly succeeded"
fi
[[ ! -e "$data_retry_cleanup" \
  && ! -e "$mock_state/container-lash-agent-workbench-dev-restate-$port_retry_cleanup" \
  && ! -e "$mock_state/container-lash-agent-workbench-dev-postgres-$port_retry_cleanup" ]] \
  || fail "successful exact-ID cleanup retry retained attempt state"
mapfile -t retry_removals < <(tail -n "+$((rm_log_before_retry + 1))" "$mock_state/docker-rm.log")
[[ "${#retry_removals[@]}" = 2 \
  && "${retry_removals[0]}" = *' restate' \
  && "${retry_removals[1]}" = *' postgres' ]] \
  || fail "cleanup retry did not prove Restate retirement before Postgres removal"

data_reset_failed_retirement="$test_tmp/data-reset-failed-retirement"
port_reset_failed_retirement=3048
run_launcher "$data_reset_failed_retirement" "$port_reset_failed_retirement" up \
  > "$test_tmp/reset-failed-retirement-up.log" 2>&1
reset_failure_state_key="127.0.0.1_${port_reset_failed_retirement}"
read -r _ reset_old_restate_id _ _ \
  < "$data_reset_failed_retirement/run/restate-$reset_failure_state_key.container"
if launcher_env "$data_reset_failed_retirement" "$port_reset_failed_retirement" \
  MOCK_POST_KILL=1 MOCK_RM_FAIL_COMPONENT=restate MOCK_RM_FAIL_MODE=always \
  MOCK_RM_ALLOW_ID="$reset_old_restate_id" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" restart --reset-dev-state \
    --port "$port_reset_failed_retirement" \
  > "$test_tmp/reset-failed-retirement.log" 2>&1; then
  fail "replacement with persistently failed Restate retirement unexpectedly succeeded"
fi
[[ -f "$data_reset_failed_retirement/attempt-app-state" \
  && -f "$data_reset_failed_retirement/.agent-workbench-dev-reset-owner" \
  && -f "$data_reset_failed_retirement/run/reset-$reset_failure_state_key.meta" \
  && -f "$data_reset_failed_retirement/run/restate-$reset_failure_state_key.container" ]] \
  || fail "replacement retirement failure deleted new application state or ownership metadata"
grep -Fq 'replacement cleanup is incomplete; retained its application state and ownership metadata' \
  "$test_tmp/reset-failed-retirement.log" \
  || fail "replacement retirement failure falsely reported a stopped stack"
! grep -Fq 'stack is stopped' "$test_tmp/reset-failed-retirement.log" \
  || fail "replacement retirement failure claimed its retained engine was stopped"
replacement_recovery_file="$(sed -n \
  's/^\[agent-workbench\] recovery command saved at \([^;]*\);.*/\1/p' \
  "$test_tmp/reset-failed-retirement.log")"
[[ -f "$replacement_recovery_file" && "$(stat -c '%a' "$replacement_recovery_file")" = 600 ]] \
  || fail "replacement retirement failure did not retain private recovery evidence"

data_external_sqlite="$test_tmp/data-external-sqlite"
port_external_sqlite=3052
external_sqlite_rm_before="$(wc -l < "$mock_state/docker-rm-attempt.log")"
if launcher_env "$data_external_sqlite" "$port_external_sqlite" \
  MOCK_EXTERNAL_PORTS='8300 19290' MOCK_POST_KILL=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_external_sqlite" \
  > "$test_tmp/external-sqlite-failure.log" 2>&1; then
  fail "external-Restate SQLite host failure unexpectedly succeeded"
fi
external_sqlite_state_key="127.0.0.1_${port_external_sqlite}"
[[ -f "$data_external_sqlite/attempt-app-state" \
  && -f "$data_external_sqlite/run/workbench-$external_sqlite_state_key.meta" ]] \
  || fail "external Restate failure deleted SQLite application state or private run metadata"
grep -Fq $'\thttp://127.0.0.1:9301' "$mock_state/deployments" \
  || fail "external Restate failure did not retain its registered deployment"
[[ "$(wc -l < "$mock_state/docker-rm-attempt.log")" = "$external_sqlite_rm_before" ]] \
  || fail "external Restate failure attempted to remove an engine"
grep -Fq 'cannot retire the external Restate engine' "$test_tmp/external-sqlite-failure.log" \
  || fail "external Restate failure did not explain retained application state"

data_external_postgres="$test_tmp/data-external-postgres"
port_external_postgres=3054
external_postgres_rm_before="$(wc -l < "$mock_state/docker-rm-attempt.log")"
if launcher_env "$data_external_postgres" "$port_external_postgres" \
  AGENT_WORKBENCH_POSTGRES=1 MOCK_EXTERNAL_PORTS='8320 19310' MOCK_POST_KILL=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_external_postgres" \
  > "$test_tmp/external-postgres-failure.log" 2>&1; then
  fail "external-Restate managed-Postgres host failure unexpectedly succeeded"
fi
external_postgres_state_key="127.0.0.1_${port_external_postgres}"
[[ -f "$data_external_postgres/attempt-app-state" \
  && -f "$data_external_postgres/run/workbench-$external_postgres_state_key.meta" \
  && -f "$data_external_postgres/run/postgres-$external_postgres_state_key.container" \
  && -f "$mock_state/container-lash-agent-workbench-dev-postgres-$port_external_postgres" ]] \
  || fail "external Restate failure deleted managed Postgres or its ownership evidence"
grep -Fq $'\thttp://127.0.0.1:9321' "$mock_state/deployments" \
  || fail "external Restate managed-Postgres failure lost its registered deployment"
[[ "$(wc -l < "$mock_state/docker-rm-attempt.log")" = "$external_postgres_rm_before" ]] \
  || fail "external Restate failure attempted dependent managed-Postgres removal"

data_external_foreground="$test_tmp/data-external-foreground"
port_external_foreground=3056
foreground_rm_before="$(wc -l < "$mock_state/docker-rm-attempt.log")"
if launcher_env "$data_external_foreground" "$port_external_foreground" \
  AGENT_WORKBENCH_POSTGRES=1 MOCK_EXTERNAL_PORTS='8340 19330' MOCK_POST_KILL=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" foreground --port "$port_external_foreground" \
  > "$test_tmp/external-foreground-failure.log" 2>&1; then
  fail "external-Restate foreground host failure unexpectedly succeeded"
fi
external_foreground_state_key="127.0.0.1_${port_external_foreground}"
[[ -f "$data_external_foreground/attempt-app-state" \
  && -f "$data_external_foreground/run/workbench-$external_foreground_state_key.meta" \
  && -f "$data_external_foreground/run/postgres-$external_foreground_state_key.container" \
  && -f "$mock_state/container-lash-agent-workbench-dev-postgres-$port_external_foreground" ]] \
  || fail "foreground external-engine failure deleted application or managed-Postgres state"
grep -Fq $'\thttp://127.0.0.1:9341' "$mock_state/deployments" \
  || fail "foreground external-engine failure lost its registered deployment"
[[ "$(wc -l < "$mock_state/docker-rm-attempt.log")" = "$foreground_rm_before" ]] \
  || fail "foreground external-engine failure attempted dependent store removal"
grep -Fq 'foreground cleanup cannot retire the external Restate engine' \
  "$test_tmp/external-foreground-failure.log" \
  || fail "foreground external-engine failure did not report retained state"

data_shared="$test_tmp/data-shared-owner"
port_shared_owner=3058
run_launcher "$data_shared" "$port_shared_owner" up > "$test_tmp/shared-owner-up.log" 2>&1
shared_pid_file="$data_shared/run/workbench-127.0.0.1_${port_shared_owner}.pid"
shared_pid_record="$(<"$shared_pid_file")"
printf 'shared owner state\n' > "$data_shared/shared-owner-state"
shared_builds_before="$(<"$mock_state/build-count")"
shared_containers_before="$(<"$mock_state/container-counter")"
alternate_run_dir="$test_tmp/alternate-shared-run"
if launcher_env "$data_shared" 3060 AGENT_WORKBENCH_RUN_DIR="$alternate_run_dir" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3060 \
  > "$test_tmp/shared-data-refusal.log" 2>&1; then
  fail "second port with the same owned data directory unexpectedly started"
fi
[[ ! -e "$alternate_run_dir" \
  && "$(<"$mock_state/build-count")" = "$shared_builds_before" \
  && "$(<"$mock_state/container-counter")" = "$shared_containers_before" \
  && "$(<"$shared_pid_file")" = "$shared_pid_record" \
  && -f "$data_shared/shared-owner-state" ]] \
  || fail "same-data refusal mutated the alternate run directory or existing stack"
grep -Fq 'data path overlaps another launcher-owned disposable stack' \
  "$test_tmp/shared-data-refusal.log" \
  || fail "same-data refusal did not identify the ownership overlap"

nested_data="$data_shared/nested-consumer"
if launcher_env "$nested_data" 3062 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3062 \
  > "$test_tmp/nested-data-refusal.log" 2>&1; then
  fail "nested data consumer unexpectedly started inside an owned data directory"
fi
[[ ! -e "$nested_data" && "$(<"$shared_pid_file")" = "$shared_pid_record" \
  && -f "$data_shared/shared-owner-state" ]] \
  || fail "nested data refusal mutated the existing stack"

shared_reset_file="$data_shared/run/reset-127.0.0.1_${port_shared_owner}.meta"
sed -i 's/^reset_schema=5$/reset_schema=4/' "$shared_reset_file"
if run_launcher "$data_shared" "$port_shared_owner" restart --reset-dev-state \
  > "$test_tmp/legacy-exclusive-lease-refusal.log" 2>&1; then
  fail "pre-exclusivity reset record unexpectedly authorized deletion"
fi
[[ "$(<"$shared_pid_file")" = "$shared_pid_record" \
  && -f "$data_shared/shared-owner-state" ]] \
  || fail "pre-exclusivity reset refusal changed the existing stack"

data_parent="$test_tmp/data-parent-owner"
data_owned_child="$data_parent/owned-child"
port_owned_child=3064
run_launcher "$data_owned_child" "$port_owned_child" up \
  > "$test_tmp/owned-child-up.log" 2>&1
owned_child_pid_file="$data_owned_child/run/workbench-127.0.0.1_${port_owned_child}.pid"
owned_child_pid_record="$(<"$owned_child_pid_file")"
printf 'owned child state\n' > "$data_owned_child/owned-child-state"
parent_builds_before="$(<"$mock_state/build-count")"
if launcher_env "$data_parent" 3066 AGENT_WORKBENCH_RUN_DIR="$test_tmp/alternate-parent-run" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3066 \
  > "$test_tmp/parent-data-refusal.log" 2>&1; then
  fail "parent data consumer unexpectedly enclosed an owned data directory"
fi
[[ ! -e "$test_tmp/alternate-parent-run" \
  && "$(<"$mock_state/build-count")" = "$parent_builds_before" \
  && "$(<"$owned_child_pid_file")" = "$owned_child_pid_record" \
  && -f "$data_owned_child/owned-child-state" ]] \
  || fail "parent data refusal mutated the existing child stack"

data_run_owner="$test_tmp/data-run-owner"
port_run_owner=3070
run_launcher "$data_run_owner" "$port_run_owner" up > "$test_tmp/run-owner-up.log" 2>&1
run_owner_pid_file="$data_run_owner/run/workbench-127.0.0.1_${port_run_owner}.pid"
run_owner_pid_record="$(<"$run_owner_pid_file")"
printf 'run owner state\n' > "$data_run_owner/run-owner-state"
run_owner_builds_before="$(<"$mock_state/build-count")"
run_owner_containers_before="$(<"$mock_state/container-counter")"
if launcher_env "$test_tmp/data-run-consumer" 3072 \
  AGENT_WORKBENCH_RUN_DIR="$data_run_owner/run" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3072 \
  > "$test_tmp/shared-run-refusal.log" 2>&1; then
  fail "disjoint data with a run directory inside owned data unexpectedly started"
fi
[[ ! -e "$test_tmp/data-run-consumer" \
  && "$(<"$mock_state/build-count")" = "$run_owner_builds_before" \
  && "$(<"$mock_state/container-counter")" = "$run_owner_containers_before" \
  && "$(<"$run_owner_pid_file")" = "$run_owner_pid_record" \
  && -f "$data_run_owner/run-owner-state" ]] \
  || fail "shared-run refusal changed the owner or candidate footprint"
grep -Fq 'run path overlaps another launcher-owned disposable stack' \
  "$test_tmp/shared-run-refusal.log" \
  || fail "shared-run refusal did not identify the complete-footprint conflict"

run_footprint_parent="$test_tmp/run-footprint-parent"
run_footprint_dir="$run_footprint_parent/owner-run"
run_footprint_data="$test_tmp/data-run-footprint-owner"
port_run_footprint_owner=3071
launcher_env "$run_footprint_data" "$port_run_footprint_owner" \
  AGENT_WORKBENCH_RUN_DIR="$run_footprint_dir" \
  MOCK_PID_FILE="$run_footprint_dir/workbench-127.0.0.1_${port_run_footprint_owner}.pid" \
  MOCK_EXTERNAL_PORTS='8490 19480' \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_run_footprint_owner" \
  > "$test_tmp/run-footprint-owner-up.log" 2>&1
run_footprint_pid_file="$run_footprint_dir/workbench-127.0.0.1_${port_run_footprint_owner}.pid"
run_footprint_pid_record="$(<"$run_footprint_pid_file")"
[[ ! -e "$run_footprint_data/.agent-workbench-dev-reset-owner" ]] \
  || fail "external-engine run unexpectedly claimed reset ownership"
printf 'external run footprint state\n' > "$run_footprint_data/run-footprint-state"
run_footprint_builds_before="$(<"$mock_state/build-count")"
if launcher_env "$run_footprint_parent" 3073 \
  AGENT_WORKBENCH_RUN_DIR="$test_tmp/run-footprint-candidate-run" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3073 \
  > "$test_tmp/enclosing-run-footprint-refusal.log" 2>&1; then
  fail "candidate data unexpectedly enclosed another stack's external run footprint"
fi
[[ ! -e "$test_tmp/run-footprint-candidate-run" \
  && "$(<"$mock_state/build-count")" = "$run_footprint_builds_before" \
  && "$(<"$run_footprint_pid_file")" = "$run_footprint_pid_record" \
  && -f "$run_footprint_data/run-footprint-state" ]] \
  || fail "enclosing-run-footprint refusal changed the owner or candidate state"
grep -Fq 'application data path encloses another launcher-owned reset footprint' \
  "$test_tmp/enclosing-run-footprint-refusal.log" \
  || fail "enclosing-run-footprint refusal did not identify the recursive deletion risk"

default_repo="$test_tmp/default-path-repo"
mkdir -p "$default_repo/scripts"
cp "$repo_root/scripts/agent-workbench-dev.sh" "$default_repo/scripts/agent-workbench-dev.sh"
default_data="$default_repo/.agent-workbench"
default_owner_port=3074
env PATH="$mock_bin:$PATH" MOCK_STATE="$mock_state" \
  MOCK_PID_FILE="$default_data/run/workbench-127.0.0.1_${default_owner_port}.pid" \
  XDG_RUNTIME_DIR="$test_tmp/runtime" CARGO_TARGET_DIR="$test_tmp/target-$default_owner_port" \
  RESTATE_ADMIN_URL=http://127.0.0.1:19510/v2 AGENT_WORKBENCH_OPEN=0 \
  AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion \
  bash "$default_repo/scripts/agent-workbench-dev.sh" up --port "$default_owner_port" \
  > "$test_tmp/default-owner-up.log" 2>&1
default_owner_pid_file="$default_data/run/workbench-127.0.0.1_${default_owner_port}.pid"
default_owner_pid_record="$(<"$default_owner_pid_file")"
default_consumer_data="$test_tmp/default-run-custom-data"
default_consumer_port=3076
default_builds_before="$(<"$mock_state/build-count")"
if env PATH="$mock_bin:$PATH" MOCK_STATE="$mock_state" \
  MOCK_PID_FILE="$default_data/run/workbench-127.0.0.1_${default_consumer_port}.pid" \
  XDG_RUNTIME_DIR="$test_tmp/runtime" CARGO_TARGET_DIR="$test_tmp/target-$default_consumer_port" \
  AGENT_WORKBENCH_DATA_DIR="$default_consumer_data" \
  RESTATE_ADMIN_URL=http://127.0.0.1:19530/v2 AGENT_WORKBENCH_OPEN=0 \
  AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion \
  bash "$default_repo/scripts/agent-workbench-dev.sh" up --port "$default_consumer_port" \
  > "$test_tmp/default-run-custom-data-refusal.log" 2>&1; then
  fail "custom data with the default run directory inside owned data unexpectedly started"
fi
[[ ! -e "$default_consumer_data" \
  && "$(<"$mock_state/build-count")" = "$default_builds_before" \
  && "$(<"$default_owner_pid_file")" = "$default_owner_pid_record" ]] \
  || fail "default-run overlap refusal changed the owner or custom data path"

data_service_owner="$test_tmp/data-service-owner"
port_service_owner=3078
run_launcher "$data_service_owner" "$port_service_owner" up \
  > "$test_tmp/service-owner-up.log" 2>&1
service_owner_pid_file="$data_service_owner/run/workbench-127.0.0.1_${port_service_owner}.pid"
service_owner_pid_record="$(<"$service_owner_pid_file")"
service_owner_restate_marker="$data_service_owner/run/restate-127.0.0.1_${port_service_owner}.container"
read -r _ service_owner_restate_id _ _ < "$service_owner_restate_marker"
printf 'service owner state\n' > "$data_service_owner/service-owner-state"
service_deployments_before="$(<"$mock_state/deployments")"
service_rm_before="$(wc -l < "$mock_state/docker-rm.log")"
service_builds_before="$(<"$mock_state/build-count")"
if launcher_env "$test_tmp/data-shared-engine-consumer" 3080 \
  RESTATE_INGRESS_URL=http://localhost:8560 \
  RESTATE_ADMIN_URL=http://localhost:19550/v2 \
  AGENT_WORKBENCH_RESTATE_NODE_PORT=19551 \
  AGENT_WORKBENCH_RESTATE_CONTAINER=alternate-shared-engine-name \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3080 \
  > "$test_tmp/shared-engine-refusal.log" 2>&1; then
  fail "second launcher unexpectedly borrowed a reset-owned Restate engine"
fi
[[ ! -e "$test_tmp/data-shared-engine-consumer" \
  && "$(<"$service_owner_pid_file")" = "$service_owner_pid_record" \
  && -f "$data_service_owner/service-owner-state" \
  && -f "$mock_state/container-lash-agent-workbench-dev-restate-$port_service_owner" \
  && "$(<"$mock_state/deployments")" = "$service_deployments_before" \
  && "$(wc -l < "$mock_state/docker-rm.log")" = "$service_rm_before" \
  && "$(<"$mock_state/build-count")" = "$service_builds_before" ]] \
  || fail "shared Restate refusal changed the owner, engine, deployment, or candidate state"
grep -Fq 'Restate service is reserved by another launcher-owned disposable stack' \
  "$test_tmp/shared-engine-refusal.log" \
  || fail "shared Restate refusal did not identify the exclusive service lease"

if launcher_env "$test_tmp/data-other-runtime-consumer" 3081 \
  XDG_RUNTIME_DIR="$test_tmp/other-runtime" \
  TMPDIR="$test_tmp/other-tmp" \
  RESTATE_INGRESS_URL=http://127.0.0.1:8560 \
  RESTATE_ADMIN_URL=http://127.0.0.1:19550/v2 \
  AGENT_WORKBENCH_RESTATE_NODE_PORT=19551 \
  AGENT_WORKBENCH_RESTATE_CONTAINER=alternate-runtime-engine-name \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3081 \
  > "$test_tmp/other-runtime-engine-refusal.log" 2>&1; then
  fail "caller-selected XDG runtime unexpectedly hid the managed Restate lease"
fi
[[ ! -e "$test_tmp/data-other-runtime-consumer" \
  && "$(<"$service_owner_pid_file")" = "$service_owner_pid_record" \
  && "$(<"$mock_state/deployments")" = "$service_deployments_before" \
  && "$(wc -l < "$mock_state/docker-rm.log")" = "$service_rm_before" ]] \
  || fail "alternate-runtime refusal changed the owner, registry, or candidate state"
grep -Fq 'Restate service is reserved by another launcher-owned disposable stack' \
  "$test_tmp/other-runtime-engine-refusal.log" \
  || fail "alternate-runtime refusal did not use the stable same-user lease namespace"

printf '19550\tdp_unexpected\thttp://127.0.0.1:9999\n' >> "$mock_state/deployments"
mixed_registry_snapshot="$(<"$mock_state/deployments")"
mixed_registry_rm_before="$(wc -l < "$mock_state/docker-rm.log")"
if run_launcher "$data_service_owner" "$port_service_owner" restart --reset-dev-state \
  > "$test_tmp/mixed-registry-reset-refusal.log" 2>&1; then
  fail "reset ignored an unexpected deployment on its managed Restate engine"
fi
[[ "$(<"$service_owner_pid_file")" = "$service_owner_pid_record" \
  && -f "$data_service_owner/service-owner-state" \
  && "$(<"$mock_state/deployments")" = "$mixed_registry_snapshot" \
  && "$(wc -l < "$mock_state/docker-rm.log")" = "$mixed_registry_rm_before" ]] \
  || fail "mixed-registry reset refusal changed a process, deployment, or engine"
grep -Fq 'deployment registry does not prove exclusive ownership' \
  "$test_tmp/mixed-registry-reset-refusal.log" \
  || fail "mixed-registry reset refusal did not report the exclusive registry proof"
awk -F '\t' '$2 != "dp_unexpected"' "$mock_state/deployments" \
  > "$mock_state/deployments.next"
mv "$mock_state/deployments.next" "$mock_state/deployments"

data_nonreset_service_owner="$test_tmp/data-nonreset-service-owner"
mkdir -p "$data_nonreset_service_owner"
port_nonreset_service_owner=3086
run_launcher "$data_nonreset_service_owner" "$port_nonreset_service_owner" up \
  > "$test_tmp/nonreset-service-owner-up.log" 2>&1
nonreset_service_pid_file="$data_nonreset_service_owner/run/workbench-127.0.0.1_${port_nonreset_service_owner}.pid"
nonreset_service_pid_record="$(<"$nonreset_service_pid_file")"
[[ ! -e "$data_nonreset_service_owner/.agent-workbench-dev-reset-owner" ]] \
  || fail "pre-existing application data unexpectedly became reset-owned"
nonreset_service_builds_before="$(<"$mock_state/build-count")"
if launcher_env "$test_tmp/data-nonreset-service-consumer" 3088 \
  RESTATE_INGRESS_URL=http://127.0.0.1:8640 \
  RESTATE_ADMIN_URL=http://127.0.0.1:19630/v2 \
  AGENT_WORKBENCH_RESTATE_NODE_PORT=19631 \
  AGENT_WORKBENCH_RESTATE_CONTAINER=alternate-nonreset-engine-name \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3088 \
  > "$test_tmp/nonreset-shared-engine-refusal.log" 2>&1; then
  fail "second launcher unexpectedly borrowed a non-resettable managed Restate engine"
fi
[[ "$(<"$nonreset_service_pid_file")" = "$nonreset_service_pid_record" \
  && ! -e "$test_tmp/data-nonreset-service-consumer" \
  && "$(<"$mock_state/build-count")" = "$nonreset_service_builds_before" ]] \
  || fail "non-resettable service refusal changed the owner or candidate state"
grep -Fq 'Restate service is reserved by another launcher-owned disposable stack' \
  "$test_tmp/nonreset-shared-engine-refusal.log" \
  || fail "non-resettable service refusal did not identify the persistent service lease"

data_database_owner="$test_tmp/data-database-owner"
port_database_owner=3082
launcher_env "$data_database_owner" "$port_database_owner" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_database_owner" \
  > "$test_tmp/database-owner-up.log" 2>&1
database_owner_pid_file="$data_database_owner/run/workbench-127.0.0.1_${port_database_owner}.pid"
database_owner_pid_record="$(<"$database_owner_pid_file")"
database_owner_postgres_marker="$data_database_owner/run/postgres-127.0.0.1_${port_database_owner}.container"
read -r _ database_owner_postgres_id _ _ < "$database_owner_postgres_marker"
printf 'database owner state\n' > "$data_database_owner/database-owner-state"
database_rm_before="$(wc -l < "$mock_state/docker-rm.log")"
if launcher_env "$test_tmp/data-shared-database-consumer" 3084 \
  AGENT_WORKBENCH_POSTGRES=1 AGENT_WORKBENCH_POSTGRES_HOST=localhost \
  AGENT_WORKBENCH_POSTGRES_PORT=15952 \
  AGENT_WORKBENCH_POSTGRES_CONTAINER=alternate-shared-database-name \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3084 \
  > "$test_tmp/shared-database-refusal.log" 2>&1; then
  fail "second launcher unexpectedly borrowed a reset-owned managed Postgres"
fi
[[ "$(<"$database_owner_pid_file")" = "$database_owner_pid_record" \
  && -f "$data_database_owner/database-owner-state" \
  && -f "$mock_state/container-lash-agent-workbench-dev-postgres-$port_database_owner" \
  && ! -e "$test_tmp/data-shared-database-consumer" \
  && "$(wc -l < "$mock_state/docker-rm.log")" = "$((database_rm_before + 1))" ]] \
  || fail "shared Postgres refusal changed the owner or retained candidate state"
! grep -Fq "rm $database_owner_postgres_id postgres" "$mock_state/docker-rm.log" \
  || fail "shared Postgres refusal removed the owner's exact database container"
grep -Fq 'Postgres service is reserved by another launcher-owned disposable stack' \
  "$test_tmp/shared-database-refusal.log" \
  || fail "shared Postgres refusal did not identify the exclusive service lease"

database_lease_hash="$(printf '%s' 'loopback:15952' | sha256sum | awk '{print $1}')"
database_lease_file="$launcher_runtime_root/postgres-$database_lease_hash.lease"
rm -f "$database_lease_file"
database_reset_rm_before="$(wc -l < "$mock_state/docker-rm.log")"
if launcher_env "$data_database_owner" "$port_database_owner" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" restart --reset-dev-state \
    --port "$port_database_owner" \
  > "$test_tmp/missing-database-lease-refusal.log" 2>&1; then
  fail "reset trusted an exact Postgres ID without its exclusive service lease"
fi
[[ "$(<"$database_owner_pid_file")" = "$database_owner_pid_record" \
  && -f "$data_database_owner/database-owner-state" \
  && -f "$mock_state/container-lash-agent-workbench-dev-postgres-$port_database_owner" \
  && "$(wc -l < "$mock_state/docker-rm.log")" = "$database_reset_rm_before" ]] \
  || fail "missing Postgres lease refusal changed the owned stack"
grep -Fq 'Postgres service lease does not prove exclusive ownership' \
  "$test_tmp/missing-database-lease-refusal.log" \
  || fail "missing Postgres lease refusal did not explain the exclusivity proof failure"

service_lease_hash="$(printf '%s' 'loopback:8560|loopback:19550' | sha256sum | awk '{print $1}')"
service_lease_file="$launcher_runtime_root/restate-$service_lease_hash.lease"
rm -f "$service_lease_file"
service_reset_rm_before="$(wc -l < "$mock_state/docker-rm.log")"
if run_launcher "$data_service_owner" "$port_service_owner" restart --reset-dev-state \
  > "$test_tmp/missing-service-lease-refusal.log" 2>&1; then
  fail "reset trusted an immutable container ID without its exclusive service lease"
fi
[[ "$(<"$service_owner_pid_file")" = "$service_owner_pid_record" \
  && -f "$data_service_owner/service-owner-state" \
  && -f "$mock_state/container-lash-agent-workbench-dev-restate-$port_service_owner" \
  && "$(wc -l < "$mock_state/docker-rm.log")" = "$service_reset_rm_before" ]] \
  || fail "missing service-lease refusal changed the owned stack"
grep -Fq 'service lease does not prove exclusive ownership' \
  "$test_tmp/missing-service-lease-refusal.log" \
  || fail "missing service-lease refusal did not explain the exclusivity proof failure"

data_ownership_lock="$launcher_runtime_root/data-ownership.lock"
exec 10> "$data_ownership_lock"
flock 10
if run_launcher "$test_tmp/data-locked-ownership" 3068 up \
  > "$test_tmp/data-ownership-lock-refusal.log" 2>&1; then
  fail "launcher ignored the same-user data-ownership lock"
fi
flock -u 10
exec 10>&-
[[ ! -e "$test_tmp/data-locked-ownership" ]] \
  || fail "data-ownership lock refusal mutated the candidate data path"
grep -Fq 'another launcher lifecycle command is updating application data ownership' \
  "$test_tmp/data-ownership-lock-refusal.log" \
  || fail "data-ownership lock refusal was not precise"

port_alias_builds_before="$(<"$mock_state/build-count")"
if launcher_env "$test_tmp/data-numeric-workbench" 3089 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 03089 \
  > "$test_tmp/numeric-workbench-refusal.log" 2>&1; then
  fail "noncanonical workbench port alias unexpectedly succeeded"
fi
if launcher_env "$test_tmp/data-numeric-endpoint" 3089 \
  AGENT_WORKBENCH_RESTATE_ADDR=127.0.0.1:09081 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3089 \
  > "$test_tmp/numeric-endpoint-refusal.log" 2>&1; then
  fail "noncanonical Restate endpoint port alias unexpectedly succeeded"
fi
if launcher_env "$test_tmp/data-numeric-ingress" 3089 \
  RESTATE_INGRESS_URL=http://127.0.0.1:08080 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3089 \
  > "$test_tmp/numeric-ingress-refusal.log" 2>&1; then
  fail "noncanonical Restate ingress port alias unexpectedly succeeded"
fi
if launcher_env "$test_tmp/data-numeric-admin" 3089 \
  RESTATE_ADMIN_URL=http://127.0.0.1:019070/v2 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3089 \
  > "$test_tmp/numeric-admin-refusal.log" 2>&1; then
  fail "noncanonical Restate admin port alias unexpectedly succeeded"
fi
if launcher_env "$test_tmp/data-numeric-node" 3089 \
  AGENT_WORKBENCH_RESTATE_NODE_PORT=019071 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3089 \
  > "$test_tmp/numeric-node-refusal.log" 2>&1; then
  fail "noncanonical Restate node port alias unexpectedly succeeded"
fi
[[ ! -e "$test_tmp/data-numeric-workbench" \
  && ! -e "$test_tmp/data-numeric-endpoint" \
  && ! -e "$test_tmp/data-numeric-ingress" \
  && ! -e "$test_tmp/data-numeric-admin" \
  && ! -e "$test_tmp/data-numeric-node" \
  && "$(<"$mock_state/build-count")" = "$port_alias_builds_before" ]] \
  || fail "noncanonical port refusal performed startup work"
grep -Fq 'workbench port must use canonical decimal notation' "$test_tmp/numeric-workbench-refusal.log" \
  || fail "workbench alias refusal omitted its canonical notation requirement"
grep -Fq 'Restate endpoint port must use canonical decimal notation' "$test_tmp/numeric-endpoint-refusal.log" \
  || fail "endpoint alias refusal omitted its canonical notation requirement"
grep -Fq 'expected URL with explicit host and port' "$test_tmp/numeric-ingress-refusal.log" \
  || fail "ingress alias refusal omitted its URL port requirement"
grep -Fq 'expected URL with explicit host and port' "$test_tmp/numeric-admin-refusal.log" \
  || fail "admin alias refusal omitted its URL port requirement"
grep -Fq 'Restate node port must use canonical decimal notation' "$test_tmp/numeric-node-refusal.log" \
  || fail "node alias refusal omitted its canonical notation requirement"

data_numeric_owner="$test_tmp/data-numeric-port-owner"
port_numeric_owner=3090
launcher_env "$data_numeric_owner" "$port_numeric_owner" \
  AGENT_WORKBENCH_POSTGRES=1 AGENT_WORKBENCH_POSTGRES_PORT=16100 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_numeric_owner" \
  > "$test_tmp/numeric-port-owner-up.log" 2>&1
numeric_owner_pid="$data_numeric_owner/run/workbench-127.0.0.1_${port_numeric_owner}.pid"
numeric_owner_pid_record="$(<"$numeric_owner_pid")"
numeric_owner_postgres="$mock_state/container-lash-agent-workbench-dev-postgres-$port_numeric_owner"
numeric_builds_before="$(<"$mock_state/build-count")"
if launcher_env "$test_tmp/data-numeric-port-consumer" 3092 \
  AGENT_WORKBENCH_POSTGRES=1 AGENT_WORKBENCH_POSTGRES_PORT=016100 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3092 \
  > "$test_tmp/numeric-port-refusal.log" 2>&1; then
  fail "noncanonical PostgreSQL port alias unexpectedly bypassed its service lease"
fi
[[ ! -e "$test_tmp/data-numeric-port-consumer" \
  && "$(<"$numeric_owner_pid")" = "$numeric_owner_pid_record" \
  && -f "$numeric_owner_postgres" \
  && "$(<"$mock_state/build-count")" = "$numeric_builds_before" ]] \
  || fail "numeric PostgreSQL port refusal changed the owner or candidate"
grep -Fq 'Postgres port must use canonical decimal notation' "$test_tmp/numeric-port-refusal.log" \
  || fail "numeric PostgreSQL port refusal omitted its canonical notation requirement"

data_external_down="$test_tmp/data-external-down"
port_external_down=3094
external_down_ingress=$((8080 + (port_external_down - 3030) * 10))
external_down_admin=$((19070 + (port_external_down - 3030) * 10))
launcher_env "$data_external_down" "$port_external_down" \
  AGENT_WORKBENCH_POSTGRES=1 MOCK_EXTERNAL_PORTS="$external_down_ingress $external_down_admin" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_external_down" \
  > "$test_tmp/external-down-up.log" 2>&1
external_down_postgres="$mock_state/container-lash-agent-workbench-dev-postgres-$port_external_down"
external_down_postgres_record="$(<"$external_down_postgres")"
external_down_deployments="$(<"$mock_state/deployments")"
if launcher_env "$data_external_down" "$port_external_down" AGENT_WORKBENCH_POSTGRES=1 \
  MOCK_EXTERNAL_PORTS="$external_down_ingress $external_down_admin" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" down --port "$port_external_down" \
  > "$test_tmp/external-down-refusal.log" 2>&1; then
  fail "targeted down reported complete retirement behind an external Restate engine"
fi
[[ -f "$external_down_postgres" \
  && "$(<"$external_down_postgres")" = "$external_down_postgres_record" \
  && "$(<"$mock_state/deployments")" = "$external_down_deployments" \
  && -f "$data_external_down/run/workbench-127.0.0.1_${port_external_down}.meta" ]] \
  || fail "targeted external-engine down deleted dependent database or ownership evidence"
grep -Fq 'retaining managed Postgres because the Restate engine is external' \
  "$test_tmp/external-down-refusal.log" \
  || fail "targeted external-engine down did not report dependent retention"

data_down_failure="$test_tmp/data-down-failure"
port_down_failure=3096
launcher_env "$data_down_failure" "$port_down_failure" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_down_failure" \
  > "$test_tmp/down-failure-up.log" 2>&1
down_failure_restate="$mock_state/container-lash-agent-workbench-dev-restate-$port_down_failure"
down_failure_postgres="$mock_state/container-lash-agent-workbench-dev-postgres-$port_down_failure"
down_failure_postgres_record="$(<"$down_failure_postgres")"
if launcher_env "$data_down_failure" "$port_down_failure" AGENT_WORKBENCH_POSTGRES=1 \
  MOCK_RM_FAIL_COMPONENT=restate MOCK_RM_FAIL_MODE=always \
  bash "$repo_root/scripts/agent-workbench-dev.sh" down \
  > "$test_tmp/down-all-failure.log" 2>&1; then
  fail "down-all reported success after persistent Restate retirement failure"
fi
[[ -f "$down_failure_restate" && -f "$down_failure_postgres" \
  && "$(<"$down_failure_postgres")" = "$down_failure_postgres_record" ]] \
  || fail "down-all removed dependent PostgreSQL after Restate retirement failed"
grep -Fq 'could not remove the exact owned restate container' "$test_tmp/down-all-failure.log" \
  || fail "down-all did not report the failed engine retirement"

data_down_success="$test_tmp/data-down-success"
port_down_success=3098
run_launcher "$data_down_success" "$port_down_success" up \
  > "$test_tmp/down-success-up.log" 2>&1
down_success_ingress=$((8080 + (port_down_success - 3030) * 10))
down_success_admin=$((19070 + (port_down_success - 3030) * 10))
down_success_lease_hash="$(printf '%s' "loopback:$down_success_ingress|loopback:$down_success_admin" | sha256sum | awk '{print $1}')"
down_success_lease="$launcher_runtime_root/restate-$down_success_lease_hash.lease"
[[ -f "$down_success_lease" ]] || fail "successful-down fixture did not create its service lease"
launcher_env "$data_down_success" "$port_down_success" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" down \
  > "$test_tmp/down-success.log" 2>&1
[[ ! -e "$down_success_lease" \
  && ! -e "$mock_state/container-lash-agent-workbench-dev-restate-$port_down_success" ]] \
  || fail "successful down-all stranded its retired Restate service lease"
data_down_reuse="$test_tmp/data-down-reuse"
launcher_env "$data_down_reuse" "$port_down_success" \
  AGENT_WORKBENCH_RUN_DIR="$test_tmp/run-down-reuse" \
  MOCK_PID_FILE="$test_tmp/run-down-reuse/workbench-127.0.0.1_${port_down_success}.pid" \
  MOCK_RESTATE_MARKER="$test_tmp/run-down-reuse/restate-127.0.0.1_${port_down_success}.container" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_down_success" \
  > "$test_tmp/down-reuse-up.log" 2>&1
pid_identity "$test_tmp/run-down-reuse/workbench-127.0.0.1_${port_down_success}.pid" \
  || fail "fresh independent stack could not reuse ports after proven down-all retirement"

race_data="$test_tmp/data-race"
race_run="$test_tmp/run-race"
race_bin="$test_tmp/race-bin"
mkdir -p "$race_bin"
cat > "$race_bin/mkdir" <<'MOCK'
#!/usr/bin/env bash
set -euo pipefail
target="${@: -1}"
if [[ -n "${RACE_DATA_DIR:-}" && "$target" = "$RACE_DATA_DIR" && ! -e "$RACE_DATA_DIR" ]]; then
  /usr/bin/mkdir -p -- "$RACE_DATA_DIR"
  printf 'foreign state\n' > "$RACE_DATA_DIR/foreign-sentinel"
  exit 1
fi
exec /usr/bin/mkdir "$@"
MOCK
chmod +x "$race_bin/mkdir"
race_builds_before="$(<"$mock_state/build-count")"
if launcher_env "$race_data" 3100 PATH="$race_bin:$mock_bin:$PATH" \
  RACE_DATA_DIR="$race_data" AGENT_WORKBENCH_RUN_DIR="$race_run" \
  MOCK_PID_FILE="$race_run/workbench-127.0.0.1_3100.pid" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port 3100 \
  > "$test_tmp/data-race-refusal.log" 2>&1; then
  fail "launcher accepted a data directory won by a competing creator"
fi
[[ "$(<"$race_data/foreign-sentinel")" = 'foreign state' \
  && "$(find "$race_data" -mindepth 1 -maxdepth 1 -printf '%f\n')" = foreign-sentinel \
  && "$(<"$mock_state/build-count")" = "$race_builds_before" ]] \
  || fail "failed admission changed a competing creator's application data"

data_pid_failure="$test_tmp/data-pid-publication"
port_pid_failure=3102
pid_publication_failure_function='BASH_FUNC_printf%%=() { if [[ "${FUNCNAME[1]-}" = write_pid_file ]]; then return 1; fi; builtin printf "$@"; }'
if launcher_env "$data_pid_failure" "$port_pid_failure" "$pid_publication_failure_function" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_pid_failure" \
  > "$test_tmp/pid-publication-failure.log" 2>&1; then
  fail "detached PID publication failure unexpectedly succeeded"
fi
spawned_pid_file="$mock_state/spawned-$port_pid_failure"
[[ -f "$spawned_pid_file" ]] && ! pid_identity "$spawned_pid_file" \
  || fail "detached PID publication failure left its captured child alive"
[[ ! -e "$mock_state/container-lash-agent-workbench-dev-restate-$port_pid_failure" ]] \
  || fail "detached PID publication failure retained its removable engine"

data_foreground_pid_failure="$test_tmp/data-foreground-pid-publication"
port_foreground_pid_failure=3104
if launcher_env "$data_foreground_pid_failure" "$port_foreground_pid_failure" \
  "$pid_publication_failure_function" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" foreground --port "$port_foreground_pid_failure" \
  > "$test_tmp/foreground-pid-publication-failure.log" 2>&1; then
  fail "foreground PID publication failure unexpectedly succeeded"
fi
foreground_spawned_pid="$mock_state/spawned-$port_foreground_pid_failure"
[[ -f "$foreground_spawned_pid" ]] && ! pid_identity "$foreground_spawned_pid" \
  || fail "foreground PID publication failure left its captured child alive"
[[ ! -e "$mock_state/container-lash-agent-workbench-dev-restate-$port_foreground_pid_failure" ]] \
  || fail "foreground PID publication failure retained its removable engine"

data_marker_failure="$test_tmp/data-marker-publication"
port_marker_failure=3106
marker_failure_path="$data_marker_failure/run/restate-127.0.0.1_${port_marker_failure}.container"
if launcher_env "$data_marker_failure" "$port_marker_failure" \
  MOCK_BLOCK_RESTATE_MARKER=1 MOCK_RESTATE_MARKER="$marker_failure_path" \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_marker_failure" \
  > "$test_tmp/marker-publication-failure.log" 2>&1; then
  fail "Restate marker publication failure unexpectedly succeeded"
fi
[[ ! -e "$mock_state/container-lash-agent-workbench-dev-restate-$port_marker_failure" ]] \
  || fail "Restate marker publication failure leaked its captured exact container"

data_missing_marker="$test_tmp/data-missing-engine-marker"
port_missing_marker=3108
if launcher_env "$data_missing_marker" "$port_missing_marker" \
  MOCK_POST_REMOVE_RESTATE_MARKER=1 MOCK_POST_KILL=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_missing_marker" \
  > "$test_tmp/missing-engine-marker-cleanup.log" 2>&1; then
  fail "post-registration missing engine marker failure unexpectedly succeeded"
fi
[[ ! -e "$mock_state/container-lash-agent-workbench-dev-restate-$port_missing_marker" \
  && ! -e "$data_missing_marker" ]] \
  || fail "in-memory engine capture did not protect missing-marker cleanup ordering"

data_down_missing="$test_tmp/data-down-missing-metadata"
port_down_missing=3110
launcher_env "$data_down_missing" "$port_down_missing" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_down_missing" \
  > "$test_tmp/down-missing-up.log" 2>&1
down_missing_pid="$data_down_missing/run/workbench-127.0.0.1_${port_down_missing}.pid"
down_missing_restate="$mock_state/container-lash-agent-workbench-dev-restate-$port_down_missing"
down_missing_postgres="$mock_state/container-lash-agent-workbench-dev-postgres-$port_down_missing"
rm -f "$data_down_missing/run/workbench-127.0.0.1_${port_down_missing}.meta"
if launcher_env "$data_down_missing" "$port_down_missing" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" down --port "$port_down_missing" \
  > "$test_tmp/down-missing-meta-refusal.log" 2>&1; then
  fail "targeted down treated missing stack metadata as retired resources"
fi
[[ ! -e "$down_missing_pid" && -f "$down_missing_restate" && -f "$down_missing_postgres" ]] \
  || fail "missing stack metadata allowed dependent service deletion"

data_down_marker="$test_tmp/data-down-missing-marker"
port_down_marker=3112
launcher_env "$data_down_marker" "$port_down_marker" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_down_marker" \
  > "$test_tmp/down-marker-up.log" 2>&1
down_marker_pid="$data_down_marker/run/workbench-127.0.0.1_${port_down_marker}.pid"
down_marker_pid_record="$(<"$down_marker_pid")"
rm -f "$data_down_marker/run/restate-127.0.0.1_${port_down_marker}.container"
if launcher_env "$data_down_marker" "$port_down_marker" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" down --port "$port_down_marker" \
  > "$test_tmp/down-missing-marker-refusal.log" 2>&1; then
  fail "targeted down treated a missing engine marker as retired"
fi
[[ "$(<"$down_marker_pid")" = "$down_marker_pid_record" \
  && -f "$mock_state/container-lash-agent-workbench-dev-restate-$port_down_marker" \
  && -f "$mock_state/container-lash-agent-workbench-dev-postgres-$port_down_marker" ]] \
  || fail "missing engine marker changed the live process or dependent services"

data_down_registry="$test_tmp/data-down-mixed-registry"
port_down_registry=3114
run_launcher "$data_down_registry" "$port_down_registry" up \
  > "$test_tmp/down-registry-up.log" 2>&1
down_registry_pid="$data_down_registry/run/workbench-127.0.0.1_${port_down_registry}.pid"
down_registry_pid_record="$(<"$down_registry_pid")"
down_registry_admin=$((19070 + (port_down_registry - 3030) * 10))
printf '%s\t%s\t%s\n' "$down_registry_admin" dp_external http://127.0.0.1:65530 \
  >> "$mock_state/deployments"
if run_launcher "$data_down_registry" "$port_down_registry" down \
  > "$test_tmp/down-mixed-registry-refusal.log" 2>&1; then
  fail "down retired a managed Restate engine with a changed deployment registry"
fi
[[ "$(<"$down_registry_pid")" = "$down_registry_pid_record" \
  && -f "$mock_state/container-lash-agent-workbench-dev-restate-$port_down_registry" ]] \
  || fail "changed-registry down altered the process or managed engine"
grep -Fq 'current Restate deployment registry does not prove exclusive ownership' \
  "$test_tmp/down-mixed-registry-refusal.log" \
  || fail "changed-registry down did not report its exclusive ownership refusal"

data_down_invalid_pid="$test_tmp/data-down-invalid-pid"
port_down_invalid_pid=3116
launcher_env "$data_down_invalid_pid" "$port_down_invalid_pid" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" up --port "$port_down_invalid_pid" \
  > "$test_tmp/down-invalid-pid-up.log" 2>&1
down_invalid_pid="$data_down_invalid_pid/run/workbench-127.0.0.1_${port_down_invalid_pid}.pid"
down_invalid_pid_record="$(<"$down_invalid_pid")"
printf 'invalid\n' > "$down_invalid_pid"
if launcher_env "$data_down_invalid_pid" "$port_down_invalid_pid" AGENT_WORKBENCH_POSTGRES=1 \
  bash "$repo_root/scripts/agent-workbench-dev.sh" down --port "$port_down_invalid_pid" \
  > "$test_tmp/down-invalid-pid-refusal.log" 2>&1; then
  fail "down treated invalid process metadata as process retirement"
fi
[[ -f "$mock_state/container-lash-agent-workbench-dev-restate-$port_down_invalid_pid" \
  && -f "$mock_state/container-lash-agent-workbench-dev-postgres-$port_down_invalid_pid" ]] \
  || fail "invalid process metadata allowed dependent service deletion"
grep -Fq 'workbench process metadata is missing or invalid' \
  "$test_tmp/down-invalid-pid-refusal.log" \
  || fail "invalid process metadata refusal did not report the failed proof"
printf '%s\n' "$down_invalid_pid_record" > "$down_invalid_pid"

printf '%s\n' 'agent-workbench explicit reset lifecycle checks passed'
