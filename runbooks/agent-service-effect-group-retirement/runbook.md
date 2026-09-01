# Operator Runbook: Stuck Effect-Group Dispatcher Retirement

> **Read [../RULES.md](../RULES.md) first.** This is an operator procedure, not an
> agent-judged browser leg. Every command runs outside the judge. Never give a judged leg
> `shell.*`, process, Docker, Restate Admin, or Restate CLI authority.

**Purpose.** Rehearse the recovery contract in
[`crates/lash-restate/README.md`](../../crates/lash-restate/README.md#stuck-effect-group-dispatcher-retirement)
against the agent-service effect-group deployment: stop its endpoint while
`EffectGroupDispatch/run` is preparing, start retirement on a replacement deployment,
kill only the dispatcher invocation ID recorded in the retired index, and prove the index,
late READY/RANK registrations, and a late payload put all retain `Retired`.

**Execution class.** Deterministic-only. This run opens no RLM session and makes no
provider call. It is listed under `deterministic_only` in `parity-matrix.toml`, produces no
dialect rows, and must never be submitted to a paid judge. Agent-service requires an
`OPENROUTER_API_KEY` at boot; use an inert value and send only the effect-group requests
named here. Any provider request invalidates the rehearsal.

## Safety and stop conditions

1. Use `restatedev/restate:1.7.0`, a unique container name, fresh explicit ports, and a
   fresh run ID. Abort if any name or port is already owned; do not take it over.
2. Record the exact agent-service PID at boot and confirm that PID is `agent-service`
   before every signal. Never use `pkill`, `killall`, a process-name match, a service name,
   a workflow key, or a wildcard as a kill target.
3. `cleanup.facts.dispatcher.id` in the pending retired index is the kill authority. It
   must exactly equal an independently described `EffectGroupDispatch/<group-key>/run`
   invocation.
4. Keep the failed deployment endpoint down. Restart the unchanged example on a new
   endpoint port and register that URL as a new revision. Otherwise Restate can redrive the
   pinned dispatcher before the index is tombstoned, proving ordinary completion instead.
5. Tail bounded log slices only. A panic, a dispatcher that reaches `ready`/`closed`, a
   non-adopted dispatcher, or an identity disagreement is Abort/RCA.

## Phase 0 — Own an isolated stack

Run from the repository root. The values below are one example; concurrent runs must use
different explicit ports and a different shell-safe slug.

```sh
run_slug=cov2
container="lash-agent-service-restate-$run_slug"
run_id="${run_slug}-retirement-witness"
group_key="agent-service:effect-group:$run_id"
group_path=${group_key//:/%3A}
run_root="$(mktemp -d "/tmp/lash-effect-group-retirement-$run_slug.XXXXXX")"
data_dir="$run_root/agent-service-data"
app_port=29200
admin_port=29270
ingress_port=29280
old_endpoint_port=29281
new_endpoint_port=29282
node_port=29122
export RESTATE_ADMIN_URL="http://127.0.0.1:$admin_port"
export RESTATE_INGRESS_URL="http://127.0.0.1:$ingress_port"
```

Before boot, require `docker inspect "$container"` to fail and use `ss -ltn` to require
all six ports above to be unbound. Then start the exact owned container:

```sh
docker run -d --name "$container" --network host \
  -e RESTATE_ADMIN__BIND_PORT="$admin_port" \
  -e RESTATE_INGRESS__BIND_PORT="$ingress_port" \
  -e RESTATE_BIND_PORT="$node_port" \
  restatedev/restate:1.7.0 | tee "$run_root/container-id.txt"
```

Poll the admin and ingress TCP ports with a 60-second deadline. On failure, save only
`docker logs --tail 80 "$container"` and abort.

Every Cargo command must source `env.sh`. Launch the old endpoint in the background with
the shell replaced by Cargo so `$!` remains the exact host PID:

```sh
(
  . ./env.sh
  export OPENROUTER_API_KEY=cov2-unused-no-provider-call
  export AGENT_SERVICE_DURABILITY=restate
  export AGENT_SERVICE_ADDR="127.0.0.1:$app_port"
  export AGENT_SERVICE_RESTATE_ADDR="127.0.0.1:$old_endpoint_port"
  export RESTATE_INGRESS_URL="http://127.0.0.1:$ingress_port"
  export AGENT_SERVICE_DATA_DIR="$data_dir"
  export AGENT_SERVICE_TRACE="$data_dir/trace.jsonl"
  exec cargo run -p agent-service --features restate --profile judged --locked -- \
    --durability restate
) >>"$run_root/agent-service.log" 2>&1 &
host_pid=$!
```

Poll both app and old endpoint ports. Require
`ps -o comm= -p "$host_pid"` to equal `agent-service`, then register and inventory the
deployment:

```sh
restate -y deployments register "http://127.0.0.1:$old_endpoint_port" \
  | tee "$run_root/register-old.txt"
restate deployments list | tee "$run_root/deployments-old.txt"
curl -fsS "http://127.0.0.1:$app_port/api/settings" \
  | tee "$run_root/settings.json"
```

Require `EffectGroupIndex`, `EffectGroupPayload`, `EffectGroupDispatch`,
`LashDurableWaitWorkflow`, and `LashDurableWaitIndex` at the old URL.

## Phase 1 — Wedge one adopted dispatcher

Submit the public #853 effect-group request in the background. In the same shell, poll
`EffectGroupIndex/$group_path/probe`; when it first reports `preparing`, confirm the exact
PID command again and send `kill -STOP "$host_pid"`. If `ready` or `closed` appears first,
continue the process and restart with a fresh run ID.

```sh
curl -sS -X POST "http://127.0.0.1:$app_port/api/effect-groups" \
  -H 'content-type: application/json' \
  --data "{\"run_id\":\"$run_id\"}" \
  -w '\nHTTP %{http_code}\n' >"$run_root/group-post.txt" 2>&1 &
group_post_pid=$!

deadline=$((SECONDS + 10))
while (( SECONDS < deadline )); do
  phase=$(curl -sS -X POST \
    "http://127.0.0.1:$ingress_port/EffectGroupIndex/$group_path/probe" 2>/dev/null || true)
  if [[ "$phase" == *'"type":"preparing"'* ]]; then
    test "$(ps -o comm= -p "$host_pid" | tr -d ' ')" = agent-service
    kill -STOP "$host_pid"
    printf '%s\n' "$phase" | tee "$run_root/stopped-phase.json"
    break
  fi
  if [[ "$phase" == *'"type":"ready"'* || "$phase" == *'"type":"closed"'* ]]; then
    exit 1
  fi
done
test -s "$run_root/stopped-phase.json"
```

Read the index while the endpoint is stopped, require `preparing` plus an `adopted`
dispatcher, and copy its exact ID:

```sh
restate state get EffectGroupIndex "$group_key" --plain \
  | tee "$run_root/wedged-index.json"
dispatcher_id=$(python3 - "$run_root/wedged-index.json" <<'PY'
import json, sys
state = json.load(open(sys.argv[1], encoding="utf-8"))["effect-group/v1/state"]
lifecycle = state["lifecycle"]
assert lifecycle["type"] == "preparing", lifecycle
dispatcher = lifecycle["dispatch"]
assert dispatcher["type"] == "adopted", dispatcher
print(dispatcher["id"])
PY
)
restate invocations describe "$dispatcher_id" \
  | tee "$run_root/wedged-dispatcher.txt"
grep -F "EffectGroupDispatch/$group_key/run" "$run_root/wedged-dispatcher.txt"
```

Only after both identity gates pass, kill the exact stopped endpoint PID. Wait for that
PID and the interrupted HTTP request; never kill either by pattern.

```sh
test "$(ps -o comm= -p "$host_pid" | tr -d ' ')" = agent-service
kill -KILL "$host_pid"
wait "$host_pid" || true
wait "$group_post_pid" || true
unset host_pid
```

## Phase 2 — Tombstone on a replacement deployment

Start the unchanged command from Phase 0 with `AGENT_SERVICE_RESTATE_ADDR` set to
`127.0.0.1:$new_endpoint_port`, retaining the same app port, ingress URL, and data dir.
Record its new exact PID as `host_pid`, poll both ports, and confirm the command. Keep the
old endpoint port down.

```sh
(
  . ./env.sh
  export OPENROUTER_API_KEY=cov2-unused-no-provider-call
  export AGENT_SERVICE_DURABILITY=restate
  export AGENT_SERVICE_ADDR="127.0.0.1:$app_port"
  export AGENT_SERVICE_RESTATE_ADDR="127.0.0.1:$new_endpoint_port"
  export RESTATE_INGRESS_URL="http://127.0.0.1:$ingress_port"
  export AGENT_SERVICE_DATA_DIR="$data_dir"
  export AGENT_SERVICE_TRACE="$data_dir/trace.jsonl"
  exec cargo run -p agent-service --features restate --profile judged --locked -- \
    --durability restate
) >>"$run_root/agent-service.log" 2>&1 &
host_pid=$!
```

After the app and new endpoint ports open, require the exact PID command again, then
register the replacement:

```sh
test "$(ps -o comm= -p "$host_pid" | tr -d ' ')" = agent-service
restate -y deployments register --force "http://127.0.0.1:$new_endpoint_port" \
  | tee "$run_root/register-replacement.txt"
restate deployments list | tee "$run_root/deployments.txt"
```

Require the replacement at the newer revision while the old URL remains in inventory.
Start the retirement saga synchronously in the background:

```sh
curl -sS -X POST \
  "http://127.0.0.1:$ingress_port/EffectGroupDispatch/$group_path/retire" \
  -H 'content-type: application/json' --data "\"$group_key\"" \
  -w '\nHTTP %{http_code}\n' >"$run_root/retirement-response.txt" 2>&1 &
retirement_curl_pid=$!
```

Poll `restate state get EffectGroupIndex "$group_key" --plain` until it reports
`lifecycle.type=retired` and `cleanup.type=pending`. Save it as
`retired-pending-index.json`; require its `cleanup.facts.dispatcher.id` to equal
`$dispatcher_id`.

```sh
deadline=$((SECONDS + 30))
while (( SECONDS < deadline )); do
  restate state get EffectGroupIndex "$group_key" --plain \
    >"$run_root/retired-pending-index.json"
  if python3 - "$run_root/retired-pending-index.json" "$dispatcher_id" <<'PY'
import json, sys
state = json.load(open(sys.argv[1], encoding="utf-8"))["effect-group/v1/state"]
lifecycle = state["lifecycle"]
pending = lifecycle.get("type") == "retired" and lifecycle.get("cleanup", {}).get("type") == "pending"
if pending:
    assert lifecycle["cleanup"]["facts"]["dispatcher"]["id"] == sys.argv[2]
raise SystemExit(0 if pending else 1)
PY
  then
    break
  fi
  sleep 1
done
python3 - "$run_root/retired-pending-index.json" <<'PY'
import json, sys
lifecycle = json.load(open(sys.argv[1], encoding="utf-8"))["effect-group/v1/state"]["lifecycle"]
assert lifecycle["type"] == "retired" and lifecycle["cleanup"]["type"] == "pending", lifecycle
PY
```

Capture the retirement invocation ID and prove the saga is still running while the exact
dispatcher is backing off against the dead old endpoint:

```sh
retirement_id=$(restate sql --json \
  "select id from sys_invocation where target_service_name = 'EffectGroupDispatch' and target_service_key = '$group_key' and target_handler_name = 'retire' order by created_at desc limit 1" \
  2>/dev/null | python3 -c 'import json,sys; rows=json.load(sys.stdin); assert len(rows)==1; print(rows[0]["id"])')
restate invocations describe "$retirement_id" | tee "$run_root/retirement-pending.txt"
restate invocations describe "$dispatcher_id" | tee "$run_root/dispatcher-backing-off.txt"
grep -F "Status:       running" "$run_root/retirement-pending.txt"
grep -F "EffectGroupDispatch/$group_key/run" "$run_root/dispatcher-backing-off.txt"
grep -F "127.0.0.1:$old_endpoint_port" "$run_root/dispatcher-backing-off.txt"
```

## Phase 3 — Kill only the cleanup-recorded invocation

This is the README escape hatch. Pass exactly the ID copied from pending cleanup:

```sh
restate -y invocation kill "$dispatcher_id" | tee "$run_root/dispatcher-kill.txt"
grep -F 'Killed 1 invocations' "$run_root/dispatcher-kill.txt"
```

Poll the retirement curl PID with a 30-second deadline, then `wait` it. Require
`retirement-response.txt` to contain `HTTP 200`. A successful kill without saga completion
is not a pass.

## Phase 4 — Prove every retained retirement fence

Require the final index probe to carry `phase.type=retired`, and require a rank-1 read to
equal `{"type":"retired"}`:

```sh
curl -sS -X POST \
  "http://127.0.0.1:$ingress_port/EffectGroupIndex/$group_path/probe" \
  | tee "$run_root/index-probe.json"
curl -sS -X POST \
  "http://127.0.0.1:$ingress_port/EffectGroupIndex/$group_path/read_rank" \
  -H 'content-type: application/json' --data '{"rank":1}' \
  | tee "$run_root/index-rank-1.json"
grep -F '"type":"retired"' "$run_root/index-probe.json"
grep -Fx '{"type":"retired"}' "$run_root/index-rank-1.json"
```

The saga journals retained fences in protocol order: READY, then ranks 1 through N. Query
only its first two `retain_resolution` inputs. Recover their exact signed key preimages and
submit fresh `await_resolution` calls; state reads alone do not prove late registration.

```sh
restate sql --json \
  "select i.target, j.entry_json from sys_invocation i join sys_journal j on i.id = j.id where i.invoked_by_id = '$retirement_id' and i.target_service_name = 'LashDurableWaitIndex' and i.target_handler_name = 'retain_resolution' and j.index = 0 order by i.created_at asc limit 2" \
  >"$run_root/ready-rank-retains.json"

python3 - "$run_root/ready-rank-retains.json" "$RESTATE_INGRESS_URL" <<'PY' | tee "$run_root/late-ready-rank.txt"
import json, sys, urllib.request
rows = json.load(open(sys.argv[1], encoding="utf-8"))
assert len(rows) == 2, rows
for label, suffix, row in zip(("READY", "RANK-1"), (":ready", ":rank:1"), rows):
    entry = json.loads(row["entry_json"])
    request = json.loads(bytes(entry["Command"]["Input"]["payload"]))
    request.pop("resolution")
    assert request["key"]["wait"]["key"].endswith(suffix), request
    object_key = row["target"].split("/", 2)[1]
    assert object_key.startswith("unscoped:"), object_key
    workflow_key = object_key.removeprefix("unscoped:")
    call = urllib.request.Request(
        f"{sys.argv[2]}/LashDurableWaitWorkflow/{workflow_key}/await_resolution",
        data=json.dumps(request, separators=(",", ":")).encode(),
        headers={"content-type": "application/json"}, method="POST")
    with urllib.request.urlopen(call, timeout=10) as response:
        result = json.load(response)
    assert result == {"status": "ok", "payload": {"type": "retired"}}, result
    print(f"late {label}: {json.dumps(result, separators=(',', ':'))}")
PY
```

Finally derive the payload object's exact address and attempt a late write:

```sh
payload_digest=$(printf '%s' "$group_key" | sha256sum | cut -d' ' -f1)
curl -sS -X POST \
  "http://127.0.0.1:$ingress_port/EffectGroupPayload/$payload_digest%3A0/put" \
  -H 'content-type: application/json' --data '{"bytes":[108,97,116,101]}' \
  | tee "$run_root/late-payload-put.json"
grep -Fx '{"type":"retired"}' "$run_root/late-payload-put.json"
```

## Phase 5 — Teardown and score

Stop only the replacement `host_pid`, wait it, and remove only the exact owned container:

```sh
test "$(ps -o comm= -p "$host_pid" | tr -d ' ')" = agent-service
kill "$host_pid"
wait "$host_pid" || true
docker rm -f "$container"
if docker inspect "$container" >/dev/null 2>&1; then exit 1; fi
```

Preserve `run_root` as evidence. It may contain infrastructure addresses and invocation
IDs; review it before sharing.

| Item | Objective gate | Evidence |
| --- | --- | --- |
| Mid-dispatch fault | preparing index had an adopted dispatcher when the exact PID died | `stopped-phase.json`, `wedged-index.json` |
| Pinned dead invocation | exact `/run` invocation backed off against the old endpoint | `dispatcher-backing-off.txt` |
| Pending tombstone | retirement ran with the same cleanup-recorded dispatcher ID | `retired-pending-index.json`, `retirement-pending.txt` |
| Exact kill | exactly one recorded invocation killed | `dispatcher-kill.txt` |
| Saga completion | retirement HTTP 200 and final retired index | `retirement-response.txt`, `index-probe.json` |
| Late READY/RANK | both late registrations resolved `Retired` | `late-ready-rank.txt` |
| Late payload put | payload object returned `Retired` | `late-payload-put.json` |
| Exact teardown | replacement PID stopped and owned container absent | final inventory |

**Pass only if every row is satisfied.**
