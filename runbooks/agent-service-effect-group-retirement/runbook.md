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
3. `lifecycle.dispatch.id` in the wedged `preparing` index (Phase 1) is the kill
   authority. It must exactly equal an independently described
   `EffectGroupDispatch/<group-key>/run` invocation. `cleanup.facts.dispatcher.id` in the
   retired index carries the same value, but `EffectGroupIndex` is a virtual object with
   exclusive handlers: the wedged `record_dispatch` holds the object lock while it backs
   off against the dead endpoint, so the queued `retire` — the handler that writes the
   tombstone — cannot run until the kill releases the lock. The tombstone is therefore
   observable only after the kill it would otherwise authorize, and Phase 2 must not wait
   for it.
4. Keep the failed deployment endpoint down. Restart the unchanged example on a new
   endpoint port and register that URL as a new revision. Otherwise Restate can redrive the
   pinned dispatcher before the index is tombstoned, proving ordinary completion instead.
5. Tail bounded log slices only. A panic, a dispatcher that reaches `ready`/`closed`, a
   non-adopted dispatcher, or an identity disagreement is Abort/RCA.

## Phase 0 — Own an isolated stack

Run from the repository root. The values below are one example; concurrent runs must use
different explicit ports and a different shell-safe slug.

`run_root` is the run's artifact directory, supplied fresh by the drive the same way every
other row in the inventory gets one — not a `/tmp` scratch dir. Phase 5 preserves it as the
evidence bundle and the scorecard cites files inside it by name, so it has to live where
the drive keeps artifacts; a `mktemp -d /tmp/...` root leaves the scorecard pointing at a
path nobody retains. It must be empty before Phase 0 and must not be inside another row's
tree.

```sh
run_slug=cov2
container="lash-agent-service-restate-$run_slug"
run_id="${run_slug}-retirement-witness"
authority_id="agent-service-runbook:$run_slug"
group_key="agent-service:effect-group:$run_id"
group_path=${group_key//:/%3A}
run_root="<fresh artifact directory for this row>"
mkdir -p "$run_root"
test -z "$(ls -A "$run_root")"
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
  export RESTATE_AUTHORITY_ID="$authority_id"
  exec cargo run -p agent-service --features restate --profile judged --locked -- \
    --durability restate
) >>"$run_root/agent-service.log" 2>&1 &
host_pid=$!
```

`RESTATE_AUTHORITY_ID` is required and must stay stable across both boots of this run,
because they share one Restate state and one data dir. Choose `authority_id` once, before
Phase 0, and export the identical value in Phase 2. Without it the binary exits
immediately with `Error: "RESTATE_AUTHORITY_ID is required for Restate durability"`.

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
this run is Abort/RCA under safety rule 5: restart with a fresh run ID.

Two different things are called "continuing the process" here, and only one of them is a
signal. The `ready`/`closed` branch is reached on a probe that did **not** trigger the
stop, so nothing is stopped yet and there is nothing to resume; what that branch owes is
reaping the backgrounded request and tearing the stack down, and `kill -CONT` there is
issued only as a no-op guard against an earlier partial attempt. The path that really can
strand a stopped process is the window **after** the stop: `kill -STOP` lands, then one of
the identity gates below fails — the `preparing`/`adopted` assertions, or the
`EffectGroupDispatch/<group-key>/run` grep — and the shell exits with `host_pid` suspended
and the container still up. Send `kill -CONT "$host_pid"` before any exit taken inside
that window, and before the teardown in Phase 5, never after: a SIGKILL to a stopped
process is delivered, but every other cleanup step that expects the app to answer is not.

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
    kill -CONT "$host_pid" 2>/dev/null || true
    kill "$group_post_pid" 2>/dev/null || true
    wait "$group_post_pid" 2>/dev/null || true
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

If either gate above fails, the process is still suspended. Resume it before you abort, so
teardown runs against a process that can answer:

```sh
kill -CONT "$host_pid"
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

## Phase 2 — Start the retirement saga on a replacement deployment

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
  export RESTATE_AUTHORITY_ID="$authority_id"
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

Require the replacement at the newer revision while the old URL remains in inventory, and
read the revision off the registration, not off the table.

`restate -y deployments register --force` states the bump itself, twice: a
`Revision: 1 -> 2` line under each service it updates, and a closing `SERVICE`/`REV` table
for the new deployment ID. That is the signal to gate on. `restate deployments list`
carries the same fact only as a bracketed suffix on each service's continuation line
(`- EffectGroupIndex [2]`), and its `CREATED-AT` column holds a bare year, so scanning that
table for a number finds `2026` on both deployments and the comparison passes on nothing.
Neither rendering is a product surface: both come from `restate-cli 1.7.0`, matching the
`restatedev/restate:1.7.0` server pinned in safety rule 1. Use the list output for the
inventory half only — both URLs still present.

```sh
grep -F 'SERVICES THAT WILL BE UPDATED:' "$run_root/register-replacement.txt"
! grep -qF 'SERVICES THAT WILL BE ADDED:' "$run_root/register-replacement.txt"
for svc in EffectGroupIndex EffectGroupPayload EffectGroupDispatch \
           LashDurableWaitWorkflow LashDurableWaitIndex; do
  grep -qE "^ $svc +2 *$" "$run_root/register-replacement.txt"
done
test "$(grep -cF 'Revision: 1 -> 2' "$run_root/register-replacement.txt")" \
  = "$(grep -cF 'Revision: ' "$run_root/register-replacement.txt")"
grep -F "http://127.0.0.1:$old_endpoint_port/" "$run_root/deployments.txt"
grep -F "http://127.0.0.1:$new_endpoint_port/" "$run_root/deployments.txt"
```

Every service the replacement registers must be an update from revision 1 to revision 2: an
`ADDED` section, or a `Revision:` line that is not `1 -> 2`, means the replacement is not
the same code at a new URL and the rehearsal is invalid.

Start the retirement saga synchronously in the background:

```sh
curl -sS -X POST \
  "http://127.0.0.1:$ingress_port/EffectGroupDispatch/$group_path/retire" \
  -H 'content-type: application/json' --data "\"$group_key\"" \
  -w '\nHTTP %{http_code}\n' >"$run_root/retirement-response.txt" 2>&1 &
retirement_curl_pid=$!
```

The index is now wedged behind the dead dispatcher and will not move until Phase 3 kills
it. Do not wait for the tombstone here.

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

## Phase 3 — Kill only the index-recorded dispatcher, then read the tombstone

This is the README escape hatch. Pass exactly the `$dispatcher_id` captured from the
wedged index in Phase 1 and already cross-checked against a described
`EffectGroupDispatch/<group-key>/run` invocation:

```sh
restate -y invocation kill "$dispatcher_id" | tee "$run_root/dispatcher-kill.txt"
grep -F 'Killed 1 invocations' "$run_root/dispatcher-kill.txt"
```

The kill releases the `EffectGroupIndex` object lock, so the queued `retire` handler runs
and writes the tombstone. Poll the index until `lifecycle.type=retired`, save it as
`retired-index.json`, and require `cleanup.facts.dispatcher.id` to equal `$dispatcher_id`.
Cleanup may already have advanced from `pending` to `complete` by the time the first read
lands; both are a pass, and the dispatcher fact is required in either shape.

```sh
deadline=$((SECONDS + 60))
while (( SECONDS < deadline )); do
  restate state get EffectGroupIndex "$group_key" --plain \
    >"$run_root/retired-index.json"
  if python3 - "$run_root/retired-index.json" "$dispatcher_id" <<'PY'
import json, sys
lifecycle = json.load(open(sys.argv[1], encoding="utf-8"))["effect-group/v1/state"]["lifecycle"]
if lifecycle.get("type") != "retired":
    raise SystemExit(1)
cleanup = lifecycle["cleanup"]
assert cleanup["type"] in ("pending", "complete"), cleanup
assert cleanup["facts"]["dispatcher"]["id"] == sys.argv[2], cleanup
raise SystemExit(0)
PY
  then
    break
  fi
  sleep 1
done
python3 - "$run_root/retired-index.json" "$dispatcher_id" <<'PY'
import json, sys
lifecycle = json.load(open(sys.argv[1], encoding="utf-8"))["effect-group/v1/state"]["lifecycle"]
assert lifecycle["type"] == "retired", lifecycle
assert lifecycle["cleanup"]["facts"]["dispatcher"]["id"] == sys.argv[2], lifecycle
PY
```

Poll the retirement curl PID with a 180-second deadline, then `wait` it. Require
`retirement-response.txt` to contain `HTTP 200`. A successful kill without saga completion
is not a pass. The saga routinely completes one to two minutes after the kill; tearing
down on a 30-second deadline destroys a run that was about to pass.

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

The two addresses are derived differently and must not be confused.
`crates/lash-restate/src/durable_wait.rs` is the live rule: the `LashDurableWaitIndex`
object key is the scope key, rendered `scope:{"version":2,"kind":"op",...}` for an
effect group's runtime-operation scope (`:242`), while the `LashDurableWaitWorkflow` key
is `sha256hex(key.key_id)` (`:195`) — a bare 64-hex digest, not the index key with a
prefix stripped.

```sh
restate sql --json \
  "select i.target, j.entry_json from sys_invocation i join sys_journal j on i.id = j.id where i.invoked_by_id = '$retirement_id' and i.target_service_name = 'LashDurableWaitIndex' and i.target_handler_name = 'retain_resolution' and j.index = 0 order by i.created_at asc limit 2" \
  >"$run_root/ready-rank-retains.json"

python3 - "$run_root/ready-rank-retains.json" "$RESTATE_INGRESS_URL" <<'PY' | tee "$run_root/late-ready-rank.txt"
import hashlib, json, sys, urllib.request
rows = json.load(open(sys.argv[1], encoding="utf-8"))
assert len(rows) == 2, rows
for label, suffix, row in zip(("READY", "RANK-1"), (":ready", ":rank:1"), rows):
    entry = json.loads(row["entry_json"])
    request = json.loads(bytes(entry["Command"]["Input"]["payload"]))
    request.pop("resolution")
    assert request["key"]["wait"]["key"].endswith(suffix), request
    object_key = row["target"].split("/", 2)[1]
    assert object_key.startswith("scope:"), object_key
    workflow_key = hashlib.sha256(request["key"]["key_id"].encode()).hexdigest()
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
| Post-kill tombstone | retired index recorded the same dispatcher ID the kill targeted | `retired-index.json`, `retirement-pending.txt` |
| Exact kill | exactly one recorded invocation killed | `dispatcher-kill.txt` |
| Saga completion | retirement HTTP 200 and final retired index | `retirement-response.txt`, `index-probe.json` |
| Late READY/RANK | both late registrations resolved `Retired` | `late-ready-rank.txt` |
| Late payload put | payload object returned `Retired` | `late-payload-put.json` |
| Exact teardown | replacement PID stopped and owned container absent | final inventory |

**Pass only if every row is satisfied.**
