# E2E Scenario: Agent Service — Restate Effect Groups

> **Read [../RULES.md](../RULES.md) first.** This scenario is API-only: every gate is an HTTP
> request and a JSON body, so drive it with an HTTP client. Boot and teardown are part of the
> run here as in every other full-host row — Phase 0 boots the stack this row owns and Phase 4
> takes it down. The agent under test is never given a host-affecting or `shell.*` tool.

**Purpose.** Prove that agent-service's public HTTP surface drives the complete Restate
effect-group choreography: the index admits one fresh group, the dispatcher starts three
children, the READY/RANK waits expose the first settlement, close cancels both losers, and
the terminal rank order remains readable through the app.

**Execution class.** Deterministic-only. The path does not open an RLM session and makes no
provider call, so dialect labels and paid model rows would describe work that never occurs.
The inventory records this scenario under `deterministic_only`; it adds no dialect-parity
rows and does not change the judged-row arithmetic.

## Scenario-specific golden rules

1. **Only the app HTTP surface is evidence.** Do not invoke `EffectGroupIndex`,
   `EffectGroupDispatch`, or a Restate admin endpoint directly. Their durable facts must be
   projected by `/api/effect-groups`. This governs evidence, not boot: Phase 0's one-time
   deployment registration is setup, and nothing it prints is a gate.
2. **One run id is one workflow.** Generate a fresh ASCII `run_id` for the row and retain it.
   Never reuse another row's id or evidence.
3. **Ranks, not scheduler timing, decide order.** Require the response's explicit ranks and
   positions. Do not infer first settlement from wall-clock timestamps.
4. **Cancellation is a terminal fact.** A closed HTTP request or an absent process is not a
   cancelled loser. Require ranked `cancelled` terminals in the response and the durable
   follow-up read.
5. **You own the stack you boot, and nothing else.** Every container name, port and directory
   in Phase 0 carries this row's own slug, and Phase 4 removes them by exact name and exact
   PID. Never `pkill`, never a process-name or port match, never a container or port another
   row owns. A name or port already in use is someone else's stack → Abort; it is not yours to
   take over, reuse, or stop.

## Stack ownership

The row needs three things and Phase 0 creates all three: an isolated Restate 1.7.0 container,
a fresh agent-service in Restate durability mode, and that host's endpoint registered with
that container. There is no prebooted stack to inherit and no external owner to ask; the
recipe below is the whole lifecycle, and Phase 4 is the other half of it.

Three tools must be on PATH before Phase 0 starts: `docker`, `cargo`, and the **`restate`
CLI** — the last one is not implied by the other two and is the only way to finish Phase 0.
Check it first (`restate --version`); discovering it missing at the registration step leaves a
booted stack with nowhere to go, and the row must then tear down and abort rather than
improvise a registration over the admin API.

The host's own stdout and stderr are retained for the whole run at `$host_log`, named in
Phase 0 and swept in Phase 4. Every other full-host runbook in this tree gates on that sweep,
and for good reason: this row's gates all read the app's own answers, so a host that panicked
between the POST and the GET can still score green on every JSON gate above it.

## Phase 0 — Boot an isolated stack, then preflight the app surface

Run from the repository root of the worktree under test. The ports below are one example;
concurrent rows must choose a different slug and different explicit ports.

```sh
run_slug="<shell-safe slug unique to this row>"
container="lash-agent-service-restate-$run_slug"
run_id="$run_slug-effect-group-$(date +%s)"
authority_id="agent-service-runbook:$run_slug"
run_root="<fresh artifact directory for this row>"
mkdir -p "$run_root"
data_dir="$run_root/agent-service-data"
host_log="$run_root/agent-service.log"
app_port="<app port>"
admin_port="<restate admin port>"
ingress_port="<restate ingress port>"
endpoint_port="<agent-service restate endpoint port>"
node_port="<restate node port>"
export RESTATE_ADMIN_URL="http://127.0.0.1:$admin_port"
export RESTATE_INGRESS_URL="http://127.0.0.1:$ingress_port"
base_url="http://127.0.0.1:$app_port"
```

Require `docker inspect "$container"` to fail and `ss -ltn` to show all five ports unbound,
then start the container this row owns:

```sh
docker run -d --name "$container" --network host \
  -e RESTATE_ADMIN__BIND_PORT="$admin_port" \
  -e RESTATE_INGRESS__BIND_PORT="$ingress_port" \
  -e RESTATE_BIND_PORT="$node_port" \
  restatedev/restate:1.7.0 | tee "$run_root/container-id.txt"
```

Poll the admin and ingress ports with a 90-second deadline; on failure save only
`docker logs --tail 80 "$container"` and abort. Then boot the host. The subshell `exec`s Cargo, and
Cargo in turn `exec`s the binary it built, so `$host_pid` ends up being the `agent-service`
process itself — that is what makes it the right thing for Phase 4 to signal. Both streams are
appended to the retained log:

```sh
(
  . ./env.sh
  export OPENROUTER_API_KEY="<inert placeholder; this row makes no provider call>"
  export AGENT_SERVICE_DURABILITY=restate
  export RESTATE_AUTHORITY_ID="$authority_id"
  export AGENT_SERVICE_ADDR="127.0.0.1:$app_port"
  export AGENT_SERVICE_RESTATE_ADDR="127.0.0.1:$endpoint_port"
  export RESTATE_INGRESS_URL="http://127.0.0.1:$ingress_port"
  export AGENT_SERVICE_DATA_DIR="$data_dir"
  export AGENT_SERVICE_TRACE="$data_dir/trace.jsonl"
  exec cargo run -p agent-service --features restate --profile judged --locked
) >>"$host_log" 2>&1 &
host_pid=$!
```

`--features restate` is not optional: both `/api/effect-groups` routes are compiled behind
that feature, so a host built without it answers 404 and the row scores a contract violation
that is really a boot mistake. `RESTATE_AUTHORITY_ID` is required under Restate durability and
must stay one value for the whole run, or the binary exits at once with `RESTATE_AUTHORITY_ID
is required for Restate durability`. The API key is inert on purpose: the binary refuses to
start without `OPENROUTER_API_KEY`, while this path opens no session and makes no provider
call, and any provider request invalidates the row.

Poll `$app_port` and `$endpoint_port` until both accept, failing the row if `$host_pid` exits
first — a host that dies during boot writes its reason to `$host_log`. Give that poll a
deadline too, generous enough to cover the build: the first Cargo-driven boot of a cold
worktree spends minutes compiling before it binds anything, so **10 minutes** rather than the
stack's 90 seconds. Without a bound the stop trigger is not real — a host that hangs without
exiting stalls the row forever instead of failing it. Then
register this host's endpoint with the container this row started:

```sh
restate -y deployments register "http://127.0.0.1:$endpoint_port" \
  | tee "$run_root/00-register.txt"
```

Record `base_url`, `run_id`, `authority_id`, `container`, every port, and `host_pid` in
`00-identities.json` before the first request; `base_url` is `<base-url>` in every phase
below, and Phase 4 signals that exact PID and removes that exact container name.

With the stack up, use an HTTP client to require `GET <base-url>/api/settings` → 200 JSON,
then request `GET <base-url>/api/effect-groups/<run-id>` and require **HTTP 400** with a body
naming the run id — that is the contract this surface implements for an unknown id, and
pinning it is the point: an unpinned "non-success" gate cannot distinguish the contract from a
shrug. Save the status and body as `00-preflight.json`. A pre-existing group under the fresh
id means the stack is not fresh → Abort.

## Phase 1 — Run the group through agent-service

Send:

```http
POST <base-url>/api/effect-groups
Content-Type: application/json

{"run_id":"<run-id>"}
```

Save the exact status and JSON body as `01-effect-group.json`. Require HTTP 200 and all of
these objective gates:

- `run_id` equals `<run-id>` and `group_key` ends with `:<run-id>`;
- `child_count == 3`, `group_admitted == true`, and `children_dispatched == true`;
- `first_settlement_rank == 1` and `first_settlement_position` equals the position carried on
  the rank-1 settlement row. This is an internal-consistency check, not an independent fact:
  the rank-1 row's position is how `first_settlement_position` is computed, so the gate can
  only fail if the server contradicts itself. Still check it, but do not report it as
  corroboration;
- `settlements` contains exactly three rows with ranks `1, 2, 3`, three distinct positions
  `{0, 1, 2}`, and strictly increasing unique `sequence` values;
- rank 1 is `completed`; ranks 2 and 3 are `cancelled`;
- `cancelled_losers == 2` and `group_terminal == true`.

The completed rank proves the RANK wait returned a stored child outcome. The two ranked
cancellations prove close wrote loser terminals rather than merely dropping local futures.

## Phase 2 — Read the durable terminal projection

Request `GET <base-url>/api/effect-groups/<run-id>` again. Save the exact JSON as
`02-durable-report.json`. A browser-rendered screenshot of that JSON is **optional** and
proves nothing the JSON does not; this row is API-only and needs an HTTP client, not a
browser. Normalize JSON object key order only, then require structural
equality with `01-effect-group.json`. Re-apply every Phase 1 rank, position, terminal, and
count gate to this independent read.

## Phase 3 — Prove the one-shot identity fence

POST the same body from Phase 1 again. Save the status and body as
`03-duplicate-refused.json`. Require a non-success response containing `already exists`.
Then GET the terminal report once more, save it as `03-post-duplicate-report.json`, and
require it still equals Phase 2 exactly. The refusal and the unchanged terminal are two halves
of one gate and need two artifacts: `03-duplicate-refused.json` witnesses only the refusal. A
duplicate that creates another run, changes a rank, or mutates a terminal is a contract
violation → Abort/RCA.

## Phase 4 — Sweep the host log and tear down

Sweep the binary's **own** output, not Cargo's. `$host_log` holds the build first and the
running host after it, so sweep from Cargo's `Running` line onward and require the result to
match nothing, saving it as `04-host-panic-sweep.txt`:

```sh
awk 'emit; /^[[:space:]]*Running /{emit=1}' "$host_log" \
  | grep -F 'panicked at' | tee "$run_root/04-host-panic-sweep.txt"
```

An unscoped grep sweeps compiler diagnostics as well, so a dependency that merely prints that
phrase in a warning fails a perfectly healthy row. A panic anywhere in the run itself is
Abort/RCA even when every gate above passed.

Then take down exactly what Phase 0 started, and nothing else:

```sh
test "$(ps -o comm= -p "$host_pid" | tr -d ' ')" = agent-service
kill "$host_pid"
wait "$host_pid" 2>/dev/null || true
docker rm -f "$container"
```

The identity check before the signal is the point: `$host_pid` is only a number, and a PID is
reused. Confirm afterwards that `$app_port` and `$endpoint_port` are unbound and that
`docker inspect "$container"` fails, and save that check — the `comm` value read before the
signal, the port probes and the failed inspect — as `04-teardown-verified.txt`. Nothing
written in Phase 0 can witness a teardown that happens here. Teardown runs the same way on
Abort as on a pass — leaving this row's host or container up is itself a finding.

## Phase 5 — Score

| Item | Objective gate | Verdict | Evidence |
| --- | --- | --- | --- |
| Fresh admission | unknown before POST; admitted after POST | | `00-preflight.json`, `01-effect-group.json` |
| Dispatch + READY | three children and `children_dispatched == true` | | `01-effect-group.json` |
| First-settlement rank | rank 1 is completed and matches `first_settlement_position` | | `01-effect-group.json` |
| Loser cancellation | ranks 2 and 3 are cancelled; `cancelled_losers == 2` | | `01-effect-group.json` |
| Terminal durability | GET exactly reproduces all three ranks and terminal facts | | `02-durable-report.json` |
| Host health | no `panicked at` in the host's own output | | `04-host-panic-sweep.txt` |
| Identity fence | duplicate refused; terminal unchanged | | `03-duplicate-refused.json`, `03-post-duplicate-report.json` |
| Teardown | this row's host PID and container gone; ports unbound | | `04-teardown-verified.txt` |

**Aggregate:** did the app's own HTTP projection prove fresh index admission, three-child
dispatch, durable first-settlement ordering, two cancelled losers, a stable terminal read,
and a one-shot workflow identity without reading any Restate service or admin endpoint
directly?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
