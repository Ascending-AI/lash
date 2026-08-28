# E2E Scenario: Agent Service — Restate Effect Groups

> **Read [../RULES.md](../RULES.md) first.** This API-only scenario is driven with browser
> HTTP primitives. It never invokes a host-affecting or `shell.*` tool. Stack creation and
> teardown belong to the external harness, not to the judge running these phases.

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
   projected by `/api/effect-groups`.
2. **One run id is one workflow.** Generate a fresh ASCII `run_id` for the row and retain it.
   Never reuse another row's id or evidence.
3. **Ranks, not scheduler timing, decide order.** Require the response's explicit ranks and
   positions. Do not infer first settlement from wall-clock timestamps.
4. **Cancellation is a terminal fact.** A closed HTTP request or an absent process is not a
   cancelled loser. Require ranked `cancelled` terminals in the response and the durable
   follow-up read.
5. **No host mutations.** The judge uses browser navigation/fetch only. A missing or unhealthy
   prebooted stack is a harness gap and triggers Abort; it is not permission to start Docker,
   run Cargo, or terminate a process from this runbook.

## Harness contract

The external harness supplies a fresh agent-service process in Restate durability mode, its
registered Restate endpoint, and `<base-url>`. It owns a unique Restate 1.7.0 container,
ports, data directory, exact-name cleanup, and process cleanup. The judge records the base
URL and a fresh `<run-id>` in `00-identities.json` before sending the first request.

## Phase 0 — Preflight the app surface

Use browser HTTP fetch to require `GET <base-url>/api/settings` → 200 JSON. Then request
`GET <base-url>/api/effect-groups/<run-id>` and require a non-success response naming that
the run id does not exist. Save the status and body as `00-preflight.json`. A pre-existing
group under the fresh id is a contaminated harness → Abort.

## Phase 1 — Run the group through agent-service

From the browser, send:

```http
POST <base-url>/api/effect-groups
Content-Type: application/json

{"run_id":"<run-id>"}
```

Save the exact status and JSON body as `01-effect-group.json`. Require HTTP 200 and all of
these objective gates:

- `run_id` equals `<run-id>` and `group_key` ends with `:<run-id>`;
- `child_count == 3`, `group_admitted == true`, and `children_dispatched == true`;
- `first_settlement_rank == 1` and `first_settlement_position` equals the position in the
  rank-1 settlement;
- `settlements` contains exactly three rows with ranks `1, 2, 3`, three distinct positions
  `{0, 1, 2}`, and strictly increasing unique `sequence` values;
- rank 1 is `completed`; ranks 2 and 3 are `cancelled`;
- `cancelled_losers == 2` and `group_terminal == true`.

The completed rank proves the RANK wait returned a stored child outcome. The two ranked
cancellations prove close wrote loser terminals rather than merely dropping local futures.

## Phase 2 — Read the durable terminal projection

Navigate to `GET <base-url>/api/effect-groups/<run-id>`. Save the exact JSON as
`02-durable-report.json` and capture the browser-rendered JSON as
`02-durable-report.png`. Normalize JSON object key order only, then require structural
equality with `01-effect-group.json`. Re-apply every Phase 1 rank, position, terminal, and
count gate to this independent read.

## Phase 3 — Prove the one-shot identity fence

POST the same body from Phase 1 again. Save the status and body as
`03-duplicate-refused.json`. Require a non-success response containing `already exists`.
Then GET the terminal report once more and require it still equals Phase 2 exactly. A
duplicate that creates another run, changes a rank, or mutates a terminal is a contract
violation → Abort/RCA.

## Phase 4 — Score

| Item | Objective gate | Verdict | Evidence |
| --- | --- | --- | --- |
| Fresh admission | unknown before POST; admitted after POST | | `00-preflight.json`, `01-effect-group.json` |
| Dispatch + READY | three children and `children_dispatched == true` | | `01-effect-group.json` |
| First-settlement rank | rank 1 is completed and matches `first_settlement_position` | | `01-effect-group.json` |
| Loser cancellation | ranks 2 and 3 are cancelled; `cancelled_losers == 2` | | `01-effect-group.json` |
| Terminal durability | GET exactly reproduces all three ranks and terminal facts | | `02-durable-report.json`, `02-durable-report.png` |
| Identity fence | duplicate POST is refused and terminal state is unchanged | | `03-duplicate-refused.json` |

**Aggregate:** did the app's own HTTP projection prove fresh index admission, three-child
dispatch, durable first-settlement ordering, two cancelled losers, a stable terminal read,
and a one-shot workflow identity without any internal hook or host-affecting judge action?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md)._
