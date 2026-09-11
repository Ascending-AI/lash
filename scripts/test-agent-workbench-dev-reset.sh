#!/usr/bin/env bash
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
test_tmp="$(mktemp -d)"
mock_bin="$test_tmp/bin"
mock_state="$test_tmp/mock-state"
mkdir -p "$mock_bin" "$mock_state" "$test_tmp/runtime"
user_runtime_mode="$(stat -c '%a' "/run/user/$UID" 2>/dev/null || true)"
if [[ -d "/run/user/$UID" && ! -L "/run/user/$UID" \
  && "$(stat -c '%u' "/run/user/$UID")" = "$UID" \
  && "$user_runtime_mode" =~ ^[0-7]{3,4}$ \
  && $((8#$user_runtime_mode & 0022)) = 0 ]]; then
  launcher_runtime_root="/run/user/$UID/lash-agent-workbench-$UID"
else
  launcher_runtime_root="/tmp/lash-agent-workbench-$UID"
fi
runtime_preexisting_entries="$test_tmp/runtime-preexisting-entries"
if [[ -d "$launcher_runtime_root" ]]; then
  find "$launcher_runtime_root" -maxdepth 1 -mindepth 1 -printf '%f\n' \
    | sort > "$runtime_preexisting_entries"
else
  : > "$runtime_preexisting_entries"
fi

cleanup() {
  local file pid start current id token component lease entry
  while IFS= read -r file; do
    read -r pid start < "$file" || continue
    current="$(awk '{print $22}' "/proc/$pid/stat" 2>/dev/null || true)"
    if [[ "$current" = "$start" ]]; then
      kill -- "-$pid" >/dev/null 2>&1 || kill "$pid" >/dev/null 2>&1 || true
    fi
  done < <(find "$test_tmp" -type f -name 'workbench-*.pid' -print 2>/dev/null)
  if [[ -d "$launcher_runtime_root" ]]; then
    for file in "$mock_state"/container-*; do
      [[ -f "$file" && "$file" != *container-counter && "$file" != *container-ports-* ]] \
        || continue
      read -r id token component < "$file" || continue
      for lease in "$launcher_runtime_root"/"$component"-*.lease; do
        [[ -f "$lease" && ! -L "$lease" ]] || continue
        if [[ "$(<"$lease")" = "1 $component $token $id" ]]; then
          rm -f "$lease"
        fi
      done
    done
    for file in "$launcher_runtime_root"/*-recover.sh; do
      [[ -f "$file" && ! -L "$file" ]] || continue
      grep -Fq "$test_tmp" "$file" && rm -f "$file"
    done
    for file in "$launcher_runtime_root"/*.lock; do
      [[ -f "$file" && ! -L "$file" && ! -s "$file" ]] || continue
      entry="${file##*/}"
      grep -Fxq "$entry" "$runtime_preexisting_entries" || rm -f "$file"
    done
  fi
  rm -rf -- "$test_tmp"
}
trap cleanup EXIT

fail() {
  printf 'FAIL: %s\n' "$*" >&2
  exit 1
}

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
sed -i 's/^reset_schema=4$/reset_schema=3/' "$shared_reset_file"
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

printf '%s\n' 'agent-workbench explicit reset lifecycle checks passed'
