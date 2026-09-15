# Operator Runbook: Attachment Condemnation, Vacuum, and Reclamation

> **Read [../RULES.md](../RULES.md) first.** This is an operator procedure, not an
> agent-judged browser leg. Every command runs outside the judge, against a host the
> operator owns. Never give a judged leg `shell.*`, process, or Docker authority.

**Purpose.** Rehearse the two levers that bound session-store growth —
session-scoped `vacuum` and mark-and-sweep attachment reclamation — against a live
Agent Workbench, and judge each outcome against the safety argument the lever
documents. The destructive half deletes bytes; the point of the rehearsal is that
an operator has read the same two refusals, the same protected zero, and the same
swept one on a host they can throw away, *before* running it on one they cannot.

**Contract cited.** `run_store_maintenance` in
[`examples/agent-workbench/src/main_sections/admin.rs`](../../examples/agent-workbench/src/main_sections/admin.rs),
over `lash::persistence::{AttachmentReclamationPolicy, EmptyRootSetPolicy,
AttachmentRootSet, MaintenanceSweep, AttachmentGcFence, VacuumReport}`.

Four facts drive every judgment below:

1. **The root set is the mark phase.** Every blob not reachable from the session
   store factory this host supplies is deleted. Pointing the sweep at the wrong
   factory deletes live content.
2. **A root set that cannot be enumerated is not an empty one.** If the mark phase
   fails, the sweep refuses rather than treating a blind read as "nothing is
   referenced". This refusal fires before grace periods and before eligibility, so
   no policy value can talk past it.
3. **An empty root set is refused by default.** A root authority that enumerated
   successfully and found zero live refs is far more often a misconfiguration than
   a deployment that truly references nothing, so the destructive reading is an
   explicit assertion (`empty_root_set: authorize_delete_all`), not a default.
4. **A freshly uploaded blob is an unscoped host put.** The workbench's upload
   endpoint writes bytes with no manifest intent, so until the user sends the turn
   that commits the ref, `grace_period_ms` is the *only* thing keeping it alive.
   A grace period shorter than the slowest upload-to-send path this deployment
   permits deletes live user content on a perfectly configured deployment, and it
   does so silently.

**Execution class.** Deterministic-only. This run opens no RLM session judged by a
model and makes no paid provider call: the workbench boots with
`AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion`, which is the
scripted provider. It is listed under `deterministic_only` in
`parity-matrix.toml`, emits no judged row, and must never be submitted to a paid
judge. Any real provider request invalidates the rehearsal.

**No UI affordance exists, deliberately.** The workbench schedules neither lever
and renders no button for either. `curl` against `/api/admin/store-maintenance`
*is* the operator surface; there is no browser leg to substitute for it.

## Safety and stop conditions

1. Use a port in the range this runbook's operator owns, a fresh
   `AGENT_WORKBENCH_DATA_DIR`, a fresh `AGENT_WORKBENCH_RUN_DIR`, and a fresh
   `RESTATE_AUTHORITY_ID`. Abort if any is already owned; do not take it over.
2. Never point this procedure at a data directory holding content you are not
   authorized to destroy. Every arm below is run against a stack created for this
   rehearsal and torn down at the end.
3. Record the exact workbench PID the launcher reports. Never use `pkill`,
   `killall`, a process-name match, or a wildcard as a kill target; teardown is
   `bash scripts/agent-workbench-dev.sh down --port <port>` **with the same `AGENT_WORKBENCH_RUN_DIR`
   and `AGENT_WORKBENCH_DATA_DIR` exported as the boot**. Without them the
   launcher looks for its stack metadata under the repo-local `.agent-workbench`
   and refuses the teardown rather than tearing down a stack it cannot prove it
   owns — correct behaviour, and a stranded container if you only learn it at
   the end.
4. Stop and RCA on any of: a `200` carrying `sweep: "swept"` where this runbook
   requires a protected zero; a non-empty `deleted_while_referenced`; a
   `root_enumeration_failure` in a phase that does not call for one; a `fence` of
   `best_effort` on a backend that should fence; or a reclaimed id that the
   retrieval endpoint still serves.
5. The request echo is part of the evidence. A `reclaimed_count: 0` under a
   week-long grace period and the same zero under a zero-length one are opposite
   findings. Never record a count without the `reclaim_policy` beside it.

## Phase 0 — Own an isolated stack

Run from the repository root of a warm workspace. The port and slug below are one
example; concurrent runs must differ in both.

```sh
. ./env.sh
port=3318
work="$(mktemp -d)"
AGENT_WORKBENCH_DATA_DIR="$work/data" \
AGENT_WORKBENCH_RUN_DIR="$work/run" \
AGENT_WORKBENCH_OPEN=0 \
AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO=valid-empty-completion \
RESTATE_AUTHORITY_ID="attachment-reclamation-$port" \
  bash scripts/agent-workbench-dev.sh up --port "$port"
```

(`just agent-workbench "$port"` is the same command; it does not export
`CARGO_TARGET_DIR`, which is why `. ./env.sh` comes first above.)

The launcher builds the binary itself before it takes any lock. This row is the one
exception to the Bazel path RULES.md describes: `AGENT_WORKBENCH_DEV_PROVIDER_SCENARIO`
turns on the `provider-wire-fixtures` feature, and a workbench built with that feature still
goes through Cargo with `--profile judged` (RULES.md, "The judged build geometry is the
shipping one"). Expect a Cargo build here, not `kiln build --config=judged`. An operator rehearsing repeatedly can point
`AGENT_WORKBENCH_BIN` at an already-built executable to skip that build; nothing
below depends on which of the two produced it.

Record the reported PID and log path in the artifact directory. Record
`GET /api/sessions` — its `current_session_id` is the session named in Phase 7.

## Phase 1 — The levers must be named

```sh
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' -d '{}'
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' \
  -d '{"reclaim_attachment":{"grace_period_ms":1000,"empty_root_set":"refuse"}}'
```

**Judge.** The first must be `400` naming both levers: a pass that asks for
nothing is a mistake, not an empty success. The second must be `422` naming the
unknown field. Both lever fields default, so a misspelling that deserialized
would report a successful pass that did none of what was asked — on a route whose
requests are composed by hand, that has to be a refusal, not a reading.

## Phase 2 — A blind root set is refused, and no policy value talks past it

Upload a small PNG and record its byte length and SHA-256. Do **not** send a turn
yet. On a stack where no turn has run, the session catalog
(`$work/data/lash-sessions/durable-core.db`) has not been created, so the mark
phase has nothing to read.

```sh
curl -s -X POST "http://127.0.0.1:$port/api/attachments" \
  -H 'content-type: application/json' \
  -d "{\"name\":\"probe.png\",\"mime\":\"image/png\",\"data_base64\":\"$(base64 -w0 probe.png)\"}"
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' \
  -d '{"reclaim_attachments":{"grace_period_ms":604800000,"empty_root_set":"refuse"}}'
```

**Judge.** `409`, and the message must say that the root set *could not be
enumerated*, that a blind root set could not be told apart from an empty one, and
that the sweep deleted nothing — naming the missing catalog path. Note what this
arm proves about ordering: the request carries a week-long grace period under
which nothing is deletion-eligible anyway, and the refusal still fires. Enumeration
failure is judged before eligibility, so an operator cannot make a blind sweep
look successful by widening the grace period.

A `200` here with `scanned_blob_count: 1, reclaimed_count: 0` would be the most
dangerous possible outcome and is Abort/RCA: it would read as a clean protected
zero while the mark phase had in fact read nothing at all.

## Phase 3 — Establish the catalog, then the empty-root refusal

Send one text-only turn. The scripted provider **does** fail the turn under this fixture —
both turns in this rehearsal settle as `turn could not be completed`, which is also why
Phase 7's `removed_node_count` is 0 while the tombstone count is 2. The catalog write is what
this step needs, not a completion. Then re-run the reclaim
at a **zero-length** grace period.

```sh
curl -s -X POST "http://127.0.0.1:$port/api/turn" \
  -H 'content-type: application/json' -d '{"text":"hello"}'
test -f "$work/data/lash-sessions/durable-core.db"
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' \
  -d '{"reclaim_attachments":{"grace_period_ms":0,"empty_root_set":"refuse"}}'
```

**Judge.** `409` again, and it must be a *different* refusal from Phase 2: the
root authority enumerated successfully and found zero live refs while a
deletion-eligible blob was present, so proceeding would have deleted every blob in
the backend. The message must point at the store factory and name
`empty_root_set=authorize_delete_all` as the explicit opt-in. Recording "409" for
both phases without the two messages collapses a working mark phase and a broken
one into one artifact, which is exactly the distinction this lever exists to draw.

Do **not** re-send with `authorize_delete_all`. On this stack the empty root set
is real — nothing has been referenced yet — and the destructive reading would be
correct-but-lucky. Phase 5 removes the emptiness instead, which is what a real
deployment's fix looks like.

## Phase 4 — The grace period is the only thing protecting an unscoped put

Same stack, same empty root set, one number changed.

```sh
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' \
  -d '{"reclaim_attachments":{"grace_period_ms":604800000,"empty_root_set":"refuse"}}'
```

**Judge.** `200`, `scanned_blob_count: 1`, `reclaimed_count: 0`,
`sweep: "nothing_to_do"`, and `reclaim_policy.grace_period_ms: 604800000` echoed
back. The root set is just as empty as it was in Phase 3 — the refusal does not
fire because no blob is deletion-eligible under a week-long grace, so there is
nothing for the empty root set to have destroyed. The blob was *seen* and *not
deleted*, and the grace period is the only reason. A reader who records this zero
without the echoed week is recording the Phase 5 finding by mistake.

## Phase 5 — A committed reference protects the blob with no grace at all

Send a turn that carries the attachment id, then re-run the *byte-identical*
Phase 3 request.

```sh
curl -s -X POST "http://127.0.0.1:$port/api/turn" -H 'content-type: application/json' \
  -d '{"text":"describe this","attachment_id":"<id>"}'
# Wait for the COMMITTED reference, not the in-flight user row: poll until
# /api/state.active_turns is empty and the attachment manifest row carries a
# non-null committed_at_ms (equivalently, until the attachment appears as a
# graph_nodes reference). committed_at_ms is what promotes the blob to a root.
# Waiting on the in-flight row instead produces a false Abort here.
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' \
  -d '{"reclaim_attachments":{"grace_period_ms":0,"empty_root_set":"refuse"}}'
```

**Judge.** The request that was `409` in Phase 3 must now be `200` with
`scanned_blob_count: 1`, `reclaimed_count: 0`, `sweep: "nothing_to_do"`. Two
different things changed together and both matter: the root set is no longer
empty, so the refusal no longer applies, *and* the blob is now reachable, so a
zero-length grace period cannot touch it.

## Phase 6 — An orphan beside a root is reclaimed, and only the orphan

Upload a second, different PNG and do not reference it. Re-run the same request.

```sh
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' \
  -d '{"reclaim_attachments":{"grace_period_ms":0,"empty_root_set":"refuse"}}'
curl -s -o referenced.png -w '%{http_code}\n' "http://127.0.0.1:$port/api/attachments/<referenced-id>"
curl -s -o /dev/null -w '%{http_code}\n' "http://127.0.0.1:$port/api/attachments/<orphan-id>"
find "$work/data/attachments" -type f
```

**Judge.** `scanned_blob_count: 2`, `reclaimed_count: 1`, `sweep: "swept"`,
`failed_count: 0`, `deleted_while_referenced: []`, `fence: "fenced"`.

Then prove the deletion in three places rather than one: the referenced id must
retrieve `200` with the **source file's** SHA-256 unchanged, the reclaimed id must
retrieve `404`, and exactly one file must remain under the blob directory. The
reclaimed blob's shard *directory* stays behind empty — count files, not
directories, or this check reads as a failed deletion.
`reclaimed_count: 1` on its own does not say *which* one; comparing bytes does.
A non-empty `deleted_while_referenced` is Abort/RCA whatever the counts say.

## Phase 7 — Vacuum is session-scoped and names its session

```sh
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' -d '{"vacuum_session_ids":["<session-id>"]}'
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' -d '{"vacuum_session_ids":["workbench-does-not-exist"]}'
```

**Judge.** The first must be `200` with a report keyed by the session id, carrying
`removed_node_count`, `removed_pending_turn_input_tombstone_count` and a `sweep`
word. Judge the `sweep` word, not the magnitudes: the counters depend on how many
turns this rehearsal happened to run, while zero counters alone cannot say whether
the pass swept nothing because there was nothing to sweep. The second must be
`404` naming the session: there is deliberately no deployment-wide form, because
the host that can justify reclaiming a session's settled rows is the one that
knows the session is not about to be resumed.

## Phase 8 — Destruction that already happened is recorded even when the pass fails

Ask for a good session and a missing one in the same request, with a sweep after
them.

```sh
curl -s -w '\n%{http_code}\n' -X POST "http://127.0.0.1:$port/api/admin/store-maintenance" \
  -H 'content-type: application/json' \
  -d '{"vacuum_session_ids":["<session-id>","workbench-missing"],
       "reclaim_attachments":{"grace_period_ms":0,"empty_root_set":"refuse"}}'
grep agent_workbench.admin.store_maintenance "$work/data/trace.jsonl" | tail -1
```

**Judge.** The response is the `404` and carries no report. The trace record must
nevertheless show `outcome: "aborted"`, the vacuum that did run listed in
`vacuumed` with its own `sweep` word, and `reclaimed_attachments: null` beside the
`reclaim_policy` that never ran. This is the arm an operator most needs to have
seen: the response alone reads as if nothing happened, the work that was already
done exists only in the trace, and the policy echo is what says the destructive
half never started. An audit trail that reports only the passes that completed
cleanly is worse than none.

## Phase 9 — Teardown

```sh
AGENT_WORKBENCH_DATA_DIR="$work/data" AGENT_WORKBENCH_RUN_DIR="$work/run" \
RESTATE_AUTHORITY_ID="attachment-reclamation-$port" \
  bash scripts/agent-workbench-dev.sh down --port "$port"
rm -rf "$work"
```

**Judge.** Teardown must not be the thing that proves the deletions: every claim
above is recorded in the artifact directory before the stack goes away.

## Scorecard

| # | Claim | Evidence |
| --- | --- | --- |
| 1 | A pass that names no lever is refused | Phase 1, `400` |
| 2 | A misspelled lever is refused, not read as "sweep nothing" | Phase 1, `422` |
| 3 | A root set that cannot be enumerated is refused, at any grace period | Phase 2, `409` naming the blind read |
| 4 | An enumerated-empty root set with a deletion-eligible blob is refused | Phase 3, a different `409` naming the opt-in |
| 5 | An unscoped host put survives on the grace period alone | Phase 4, `nothing_to_do` with the echoed week |
| 6 | A committed reference protects a blob at zero grace | Phase 5, the identical request now `200`/`0` |
| 7 | The sweep deletes the orphan and only the orphan | Phase 6, counts plus a byte comparison plus a `404` |
| 8 | Vacuum is session-scoped and refuses an unknown session | Phase 7, `200` keyed report and `404` |
| 9 | Mid-sequence failure still records what was destroyed | Phase 8, `outcome: "aborted"` in the trace |

Any row without its own recorded artifact is not scored. A scorecard completed
from the procedure rather than from the run is the failure this layer exists to
prevent.
