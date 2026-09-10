# E2E Scenario: Version Bump By Store Recreation

> **Read [../RULES.md](../RULES.md) first.** This is the agent-judged semantic layer over
> the deterministic `just version-bump-recreation-e2e` companion. Do not replace the
> companion's PostgreSQL assertions with manual SQL, and do not treat a green script as the
> judgment itself.

**Purpose.** Prove one unsupported-schema recreation path end to end. A store with live
sessions, processes, and triggers is refused when no explicit migration applies; this
binary also refuses a store stamped newer than it expects; recreation removes the seeded
rows; and the scenario host verifies sessions, background processes, and triggers before
it reopens ingress. Lash supports explicitly declared migrations, so a changed schema is
not by itself a recreation requirement.

**Deterministic companion.** Run with a fresh artifact directory:

```sh
LASH_VERSION_BUMP_ARTIFACT_DIR=<fresh-dir> just version-bump-recreation-e2e
```

The companion owns one PostgreSQL service on the worktree's deterministic host-port offset
**+47** under the fixed `lash-version-bump-<worktree-slug>` compose project on the external
`lash-e2e-<worktree-slug>` network (created idempotently, never destroyed), and removes the
project's services and volume on exit. `LASH_VERSION_BUMP_POSTGRES_PORT` remains an explicit
port override. Its path-qualified project and ownership label prevent it from touching another
worktree's PostgreSQL service. It seeds the pre-bump deployment with a real turn
per session, a live background process holding a pending wake, and a fired trigger
delivery, then rewinds the recorded component schema version by one. It emits
`version-bump recreation e2e passed: phases=4 refusal_cases=3` only after every phase
assertion holds.
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
   direction proves binary/store incompatibility in the downgrade direction; it says
   nothing about restoring a host-owned backup.
2. **This refusal requires recreation because no migration applies.** After the bump, no
   seeded session row, process row, or committed graph node survives. An explicit supported
   migration is a different path and must not be generalized into this procedure.
3. **Verification precedes ingress in this fixture's host policy.** The health phase reuses the pre-bump session ids:
   host-chosen identifiers survive a bump even though their rows do not. All three
   surfaces gate independently. Two out of three is a failed bump.
4. **Every claim needs observed evidence.** Each bump conclusion is scored against a
   companion artifact. A conclusion with no evidence behind it is a finding, not a pass by
   default.
5. **A probe that refuses everything proves nothing.** The readability probe is judged in
   both directions or not at all: it must report *ready* on the store this build just wrote
   and on the store the recreation produced, and *refused* on each of the three fixtures.
   A run that only checked the refusals has evidence for a broken probe and a correct one
   alike.
6. **Rollback and backup policy are outside this scenario.** Do not restore, downgrade, or
   re-stamp a version during the run. The newer-store fixture proves an exact-version
   refusal; it does not prove or prescribe a host's backup-based rollback procedure.

## Recreation procedure and evidence

- Companion command and artifacts, from the repository root, as above.
- Use the read-only preflight before opening the store. An exact match is ready and a
  mismatch names found and expected versions. Then inspect the open refusal; follow this
  recreation path only when it says no explicit migration applies.
- Drain or stop the host-owned work that can still be drained, then stop every writer to the
  trust domain. Recreate the whole Lash trust domain together: session tombstones,
  await-event revocation state, effect journal, and any Restate state share the session-id
  lifecycle and must not be reset independently. This PostgreSQL fixture observes only its
  Lash-owned database objects; it does not exercise a separately deployed Restate journal.
- Open the empty store with the new binary, rerun the read-only probe, and apply the host's
  chosen health gates before reopening ingress. This fixture chooses session, process/wake,
  and trigger gates; Lash does not mandate that exact three-gate checklist.
- Backup, restore, and rollback are host policy and are not tested here. Save the completed
  scorecard in the artifact directory, and do not edit the runbook, companion, or artifacts
  during judgment.

## Phase 0 — Boot and establish the pre-bump deployment

Run the deterministic companion. Require all of these before judging later phases:

- `00-live-services.json` contains a running PostgreSQL service;
- `00-postgres-service.json` identifies the container publishing the assigned port;
- `00-postgres.json` reports that assigned port; and
- `01-seed.jsonl` carries `seeded_older_deployment` with two session ids, one live process
  id, one reserved trigger delivery, `committed_sessions` equal to the session count, a
  pending wake sequence, and a `recorded_version` exactly one below `expected_version`; and
- that same checkpoint carries `probe_before_rewind` and `probe_after_rewind`, two deep
  readability probes over the same durable bytes with only the schema stamp moved between
  them.

**Fail if:** PostgreSQL is exposed on a host port other than the assigned derived or
explicitly overridden port, a seeded session shows no
committed content (`committed_sessions` below the session count, or `committed_nodes` at
zero), the seeded process is already terminal, or the script leaves its compose project
running after exit.

**Also fail if** `probe_before_rewind` is anything but `ready` with an empty `drain` list
(the probe refusing a store this build wrote minutes earlier is a defect in the probe, not
a finding about the store), or if `probe_after_rewind` reports drain blockers. Moving the
schema stamp alone must flip the outcome to `refused` and leave the drain list empty: the
deployment cannot open, and it holds nothing that could not be carried across. A run where
both changed has lost the ability to tell those two conditions apart, which is the whole
purpose of the drain list.

## Phase 1 — Both refusal directions

**Setup.** `02-refusal.jsonl` records three open attempts against the same database.

**Action.** Read the `refused_divergent_store`, `refused_older_store`, and
`refused_newer_store` checkpoints, and the verbatim error and `refusal_kind` each carries.

**Expected observable evidence.** No attempt opened the store. This destructive generation
has no migration arm, so the historically named `refused_divergent_store` checkpoint is
`no_applicable_migration` and carries an empty `divergent_artifacts` list. It names the
immediate predecessor as found and the current version as expected. The genuinely older
store is below every explicit migration source and carries the same refusal kind. The
newer-store refusal also carries that kind, names a version one above expected as found,
and reports the same expected value, which models this binary meeting a store created by a
newer one.

Each checkpoint also carries a `probe` report, taken in summary mode (the shape a host runs
at boot) against the same fixture the open then refused. Each must report `refused`,
must name the same found and expected versions the refusal names, and must list the
per-session walk it skipped under `not_scanned` with the checkpoint-manifest row reading
`not_scanned` rather than `empty`.

**Judgment — FAIL if:** any attempt succeeded, a refusal omits the found or expected
version, a probe passed a store the open then refused or disagreed with the refusal about
either version, a summary-mode probe reported a surface it never walked as empty, a
refusal's `refusal_kind` is not the one its direction exists to prove (a non-empty refusal
of the wrong kind is not evidence for that direction), the refusals
disagree about the expected version, or the newer-store direction is missing (the run then
has not proved the symmetric exact-version gate).

## Phase 2 — The recreation bump

**Setup.** `03-recreation.jsonl` records the state before and after the bump.

**Action.** Read `recreated_store`: the pre-bump row counts, the number of lash-owned
tables dropped, the version recorded by the first open of the empty database, and the
survival counts.

**Expected observable evidence.** The pre-bump store held sessions, processes, and
committed graph nodes; the recreation dropped every lash-owned table; the recreated store
records exactly the version the refusals named as expected; `surviving_seeded_rows` and
`surviving_seeded_graph_nodes` are both `0`.

The checkpoint also carries a deep `probe` of the recreated store, which must report
`ready` with a matching schema and an empty drain list. This is the control for Phase 1: a
probe that refused every fixture and this store too would have satisfied every refusal
assertion while being useless as a deploy gate.

**Judgment — FAIL if:** the probe of the recreated store is anything but ready with an
empty drain list, the recreated store records any other version, a seeded session,
process, or committed node survives, or this no-applicable-migration fixture needed an
unexpected migration, a manual `lash_schema_versions` edit, or a table-level fixup.

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

## Phase 4 — Judge the recreation path from observed behavior

Correlate the four artifact files; do not accept the companion's pass line as the judgment.
The seeded ids must connect the pre-bump rows, destruction proof, and fresh health checks,
while found/expected versions must agree between every refusal and its probe.

| Required operator conclusion | Independent behavior evidence |
|---|---|
| An exact-match gate refuses a store in either direction, naming found and expected | `02-refusal.jsonl` (all three checkpoints) |
| This fixture uses recreation only after a `no_applicable_migration` refusal | older-store refusal and `premise_refusal_kind` in `03-recreation.jsonl` |
| This destructive generation has no migration arm, including from its immediate predecessor | `refused_divergent_store` reports `no_applicable_migration` with an empty artifact list |
| Recreation destroys the seeded PostgreSQL state | seed ids/counts versus `03-recreation.jsonl` dropped-table and zero-survivor facts |
| This binary refuses a store stamped one version newer | `02-refusal.jsonl` (`refused_newer_store`), without inferring a backup/rollback policy |
| This fixture verifies sessions, processes, and triggers before its host would reopen ingress | `04-health.jsonl` uses the seeded ids and all three gates pass |
| A read-only probe answers the readability question before the store is opened, and names what it did not read | `01-seed.jsonl`, `02-refusal.jsonl` probe reports |

Missing fields, inconsistent identities or versions, or a conclusion that requires facts
outside the artifact bundle are failures. Preserve the bundle and report the unsupported
claim. In particular, this run does not prove that Lash supplies no backup/restore facility
or that rollback is impossible; neither claim is part of the score.

The store/journal coupling the checklist calls out is only partly observable here: this
scenario runs no workflow engine, so it evidences the store half (recreated rows are gone,
so anything replaying against them refers to rows that no longer exist) and leaves the
journal half to the Restate scenarios. Say so in the scorecard rather than claiming the
coupling was tested end to end.

## Phase 5 — Teardown and score

Require the companion's final `panic gate: clean` and
`version-bump recreation e2e passed: phases=4 refusal_cases=3` lines, and confirm its compose project
and volume no longer exist.

| Item | Objective gate | Verdict | Evidence |
|------|----------------|---------|----------|
| Pre-bump deployment | Assigned PostgreSQL port live; live sessions, process, trigger, and a rewound version | | `00-*`, `01-seed.jsonl` |
| Immediate-predecessor refusal | destructive generation refuses as `no_applicable_migration` with no divergence artifacts | | `02-refusal.jsonl` |
| Older-store refusal | open refused as `no_applicable_migration`; found and expected versions named | | `02-refusal.jsonl` |
| Newer-store refusal | a store one version ahead is refused identically | | `02-refusal.jsonl` |
| Recreation bump | every lash table dropped; fresh open records the expected version | | `03-recreation.jsonl` |
| Destroyed state | zero surviving seeded rows or committed nodes | | `03-recreation.jsonl` |
| Post-bump health | turns committed, wake delivered and terminal, one reserved trigger delivery finished | | `04-health.jsonl` |
| Readability probe | ready on both clean stores, refused on all three fixtures, versions agreeing with each refusal | | `01-seed.jsonl`, `02-refusal.jsonl`, `03-recreation.jsonl` |
| Drain preflight | schema-only divergence produced no drain blockers | | `01-seed.jsonl` probe pair |
| Procedure judgment | every Phase 4 conclusion matched independent observed evidence | | four artifact files, completed scorecard |
| Teardown | panic gate clean; no owned containers or volumes remain | | `version-bump-recreation-e2e.log`, container inventory |

**Aggregate:** would an operator following only this self-contained procedure recognize the
no-migration refusal, recreate the in-scope trust domain, verify the new store, and avoid
claiming an untested backup or rollback guarantee?

---

_Stop triggers and the Abort/RCA + reporting protocol are in [../RULES.md](../RULES.md). A
failing live scenario is a product finding: preserve the artifact directory and stop; never
loosen an assertion or rewrite the judgment criterion during that run._
