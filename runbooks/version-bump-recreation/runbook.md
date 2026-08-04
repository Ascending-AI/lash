# E2E Scenario: Version Bump By Store Recreation

> **Read [../RULES.md](../RULES.md) first.** This is the agent-judged semantic layer over
> the deterministic `just version-bump-recreation-e2e` companion. Do not replace the
> companion's PostgreSQL assertions with manual SQL, and do not treat a green script as the
> judgment itself.

**Purpose.** Prove that the documented bump story is the behavior lash actually has: an
older-schema store with live sessions, processes, and triggers is refused rather than
migrated; a recreated store is refused by the version that came before it, so the bump is
forward-only with nothing to roll back to; and after the recreation the three durable
surfaces (sessions, background processes, triggers) work on the new store. The judgment is
a comparison: every claim the published operations page makes about bumping must match
what the companion observed.

**Deterministic companion.** Run with a fresh artifact directory:

```sh
LASH_VERSION_BUMP_ARTIFACT_DIR=<fresh-dir> just version-bump-recreation-e2e
```

The companion owns one PostgreSQL service on host port **5463** under a per-invocation
compose project, and removes that project and its volume on exit. It never touches another
worktree's assigned PostgreSQL service. It seeds the pre-bump deployment with a real turn
per session, a live background process holding a pending wake, and a fired trigger
delivery, then rewinds the recorded component schema version by one. It emits
`version-bump recreation e2e passed: scenarios=4` only after every phase assertion holds.
Its `0*`-prefixed artifacts are the backend truth for this judged runbook.

**No real tokens.** The companion's turns run against a deterministic in-process provider
that returns one fixed Lashlang program. Do not configure a live provider for this
scenario.

**Fixture honesty.** The pre-bump store is created by the current binary and then stamped
with the previous component version. The recorded version *is* the entire gate lash
enforces at open, so the rewind reproduces an older deployment exactly at the point under
test; it does not reproduce an older release's table shapes, and no claim in this runbook
depends on that. Treat any judgment that needs the old table shapes as out of scope.

## Scenario-specific golden rules

1. **The gate is symmetric.** A recorded version below *and* above the expected one is
   refused, and each refusal names the version found and the version expected. The second
   direction is the forward-only claim; a run that only proves the older-store refusal has
   not tested the policy.
2. **Recreation is destructive, and that is the documented contract.** After the bump, no
   seeded session row, process row, or committed graph node survives. A run that finds
   pre-bump rows on the recreated store has found a migration lash does not have.
3. **Verification precedes ingress.** The health phase reuses the pre-bump session ids:
   host-chosen identifiers survive a bump even though their rows do not. All three
   surfaces gate independently. Two out of three is a failed bump.
4. **Docs claims are assertions.** Each documented statement about bumping is scored
   against a companion artifact. A claim with no evidence behind it is a finding against
   the docs, not a pass by default.
5. **No rollback leg exists, and none may be invented.** Do not restore, downgrade, or
   re-stamp a version to make a previous binary open a recreated store. Attempting one is
   outside the scenario and voids the run.

## Working material

- Companion command and artifacts, from the repository root, as above.
- Docs surface: serve the checked-in `docs/` directory on an unused loopback port, open
  `/operations.html`, and stop the server during teardown. The server only exposes static
  in-repo files.
- Release-notes rule: `docs/PUBLISHING.md`, section "Releases that require store
  recreation".
- Save command output and rendered text in the run artifact directory. Do not edit docs or
  sources during a judged run; a divergence is a finding.

## Phase 0 — Boot and establish the pre-bump deployment

Run the deterministic companion. Require all of these before judging later phases:

- `00-live-services.json` contains a running PostgreSQL service;
- `00-postgres-service.json` identifies the container publishing port `5463`;
- `00-postgres.json` reports port `5463`; and
- `01-seed.jsonl` carries `seeded_older_deployment` with two session ids, one live process
  id, one reserved trigger delivery, `committed_sessions` equal to the session count, a
  pending wake sequence, and a `recorded_version` exactly one below `expected_version`.

**Fail if:** PostgreSQL is exposed on another host port, a seeded session shows no
committed content (`committed_sessions` below the session count, or `committed_nodes` at
zero), the seeded process is already terminal, or the script leaves its compose project
running after exit.

## Phase 1 — Both refusal directions

**Setup.** `02-refusal.jsonl` records two open attempts against the same database.

**Action.** Read the `refused_older_store` and `refused_newer_store` checkpoints, and the
verbatim error each carries.

**Expected observable evidence.** Neither attempt opened the store. The older-store
refusal names the seeded version as found and one higher as expected. The newer-store
refusal names a version one above expected as found and reports the same expected value,
which is the current binary standing in for the previous image meeting a recreated store.

**Judgment — FAIL if:** either attempt succeeded, a refusal omits the found or expected
version, the two refusals disagree about the expected version, or the newer-store
direction is missing (the run then proves reject-and-recreate but not forward-only).

## Phase 2 — The recreation bump

**Setup.** `03-recreation.jsonl` records the state before and after the bump.

**Action.** Read `recreated_store`: the pre-bump row counts, the number of lash-owned
tables dropped, the version recorded by the first open of the empty database, and the
survival counts.

**Expected observable evidence.** The pre-bump store held sessions, processes, and
committed graph nodes; the recreation dropped every lash-owned table; the recreated store
records exactly the version the refusals named as expected; `surviving_seeded_rows` and
`surviving_seeded_graph_nodes` are both `0`.

**Judgment — FAIL if:** the recreated store records any other version, a seeded session,
process, or committed node survives, or the bump needed a step the docs do not describe (an
explicit migration, a manual `lash_schema_versions` edit, or a table-level fixup).

## Phase 3 — Post-bump health on the recreated store

**Setup.** `04-health.jsonl` carries `verified_recreated_deployment`.

**Action.** Read the three independent gates and the facts behind them: the committed
session count and node count against the reused session ids, the wake enqueue and its
arrival in the target session's queued work plus the process's terminal status, and the
trigger's reservation count and delivered process status.

**Expected observable evidence.** `session_turn_committed`, `process_ran_to_terminal`, and
`trigger_fired` are each `true`; `session_ids_reused` equals the seeded session ids;
`wake_enqueued` is `1` with `wake_delivered_to_target` true and a `Completed`
`process_status`; `trigger_reservations` is `1` and `trigger_process_status` is
`Completed`.

**Judgment — FAIL if:** any gate is false, the health phase used different session ids
than the seed, the wake reached a session other than its target, or the fired occurrence
reserved a delivery whose process never reached a terminal.

## Phase 4 — Score the documented claims against the observed run

Serve `docs/` on loopback and open `/operations.html`. Poll until the **Schema
Compatibility** and **Bumping lash** sections render, then score each claim below against
the named artifact. Save the rendered text of both sections as `05-docs-claims.txt` and
capture `05-bump-policy.png` with the forward-only contract and the checklist visible.

| Documented claim | Evidence |
|---|---|
| An exact-match gate refuses a store in either direction, naming found and expected | `02-refusal.jsonl` (both checkpoints) |
| Adopting a changed schema means recreating the store from empty | `03-recreation.jsonl` |
| The previous binary refuses the recreated store, so there is no rollback | `02-refusal.jsonl` (`refused_newer_store`) |
| Recreation destroys the deployment's durable state | `03-recreation.jsonl` survival counts |
| Verification covers sessions, processes, and triggers before ingress reopens | `04-health.jsonl` |
| The page states no backup or restore ships with lash | rendered page; absence of any restore step in this runbook |

Also confirm `docs/PUBLISHING.md` requires a `Breaking:` release note that names
recreation, states forward-only with no rollback, and points at the checklist. A page that
promises a procedure the companion never performed, or a companion step the page omits, is
a **contract violation** between docs and behavior: report it as a finding.

The store/journal coupling the checklist calls out is only partly observable here: this
scenario runs no workflow engine, so it evidences the store half (recreated rows are gone,
so anything replaying against them refers to rows that no longer exist) and leaves the
journal half to the Restate scenarios. Say so in the scorecard rather than claiming the
coupling was tested end to end.

## Phase 5 — Teardown and score

Stop the static docs server and confirm its loopback port is closed. Require the
companion's final `panic gate: clean` and
`version-bump recreation e2e passed: scenarios=4` lines, and confirm its compose project
and volume no longer exist.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Pre-bump deployment | PostgreSQL:5463 live; live sessions, process, trigger, and a rewound version | | `00-*`, `01-seed.jsonl` |
| Older-store refusal | open refused; found and expected versions named | | `02-refusal.jsonl` |
| Forward-only refusal | a store one version ahead is refused identically | | `02-refusal.jsonl` |
| Recreation bump | every lash table dropped; fresh open records the expected version | | `03-recreation.jsonl` |
| Destroyed state | zero surviving seeded rows or committed nodes | | `03-recreation.jsonl` |
| Post-bump health | turns committed, wake delivered and terminal, one reserved trigger delivery finished | | `04-health.jsonl` |
| Docs agreement | every scored claim matched an artifact | | `05-docs-claims.txt`, `05-bump-policy.png` |
| Teardown | panic gate clean; no owned containers or volumes remain | | `version-bump-recreation-e2e.log`, container inventory |

**Aggregate:** would an operator who followed only the published checklist have completed
this bump, and would that operator have been correctly warned that the previous version
cannot be redeployed once the first store was recreated?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md). A
failing live scenario is a product finding: preserve the artifact directory and stop; never
loosen an assertion or rewrite the judgment criterion during that run._
