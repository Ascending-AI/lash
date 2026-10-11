# Recorded crash-resume — Pass

Date: 2026-10-11. Baseline: `26f00498dd871f9f60cda9a6a266ac43a6f29849` plus the FIG-5987 changes in this commit. Evidence root: `.kiln/FIG-5987/`. Cost: **$0**, deterministic scripted provider. Judged build: `judged-build.json`; final fixture build log: `judged-build-reply.log`.

Scripted cell SHA-256: `3d28bb9ca620bf86e0b84291df4aed78425d98d8d09c8f1c1703747481d904eb`. Store: SQLite file; dialect: TypeScript. Recorder: `record-crash.py`; artifact scoring: `score-crash.py`. Captured state was scored without rerunning either kill arm.

| Arm | Laws executed/passed | Owned SIGKILL | Session/turn/cell | Hashes and sizes | Once body/completion | Graph reuse | Reload/teardown | Verdict |
| --- | --- | --- | --- | --- | --- | --- | --- | --- |
| Host | 6/6 shared companions | host 395368; restarted host 401337 | same; one durable cell | equal; text 204800 bytes; array 131072 numbers in four chunks | 1/1, same call id | 1257 nodes / 2694890 bytes retained | both observed | Pass |
| Worker | 6/6 shared companions | worker only; exact PID in 02-killed-process.json; host unchanged | same; one durable cell | equal, same sizes | 1/1, same call id | 1257 nodes / 2694890 bytes retained | both observed | Pass |

Each arm retained 1128312 bytes of large StructureBlocks and three large
TextLeaf/BytesLeaf nodes unchanged. Each added 48 nodes / 1061198 bytes and
retired 73 nodes / 683656 bytes; net growth 377542 bytes. No new large text or
byte leaves appeared. Cell retirement replaces frame/control nodes; a final
store is not expected to retain every obsolete node.

Each trace has **two start observations, one distinct durable cell identity,
one successful completion, one transcript code row, one input and one reply**.
The start observation is emitted before durable invocation lookup
(`crates/lash-core/src/runtime/turn_driver/handlers.rs`), so re-entry repeats it.
The original scorer's raw-start-count Abort is retained as
`scorecard-original-start-count.json`; identity proof is `03-identity-proof.json`.

Artifacts: `run-root/workbench-crash-resume/typescript/sqlite/{host,worker}/`.
Each holds PID/start-time/command evidence, parked snapshot/wait/store,
binding hashes, Once ledger, checkpoint growth, state/trace, screenshots,
replacement-worker status and teardown. Overall artifact scorecard is Pass.

Companion receipts (each executed=1, passed=1): `crash-once-report.json`,
`crash-large-shared-report.json`, `crash-large-fresh-report.json`,
`crash-sleep-report.json`, `crash-worker-report.json`, `crash-round-report.json`.
Full selectors and targets are in the runbook and `proof-results.json`.

The Lash hosts and test clients ran under `ulimit -v 8388608`. Observed host
VmHWM: 527624 KiB (host resume), 547476 KiB (worker resume). Chromium ran
uncapped. Dedicated `/usr/bin/time -v` read-only reload measurements:
104572 KiB (host), 103420 KiB (worker), in `09-browser-rss.log`; no input or
kill was repeated. The original timed recorder's process-tree peak was
1030180 KiB, which includes its large trace traversal and is not browser RSS.

Fixture preflights retained as findings: Array.fill exhausted the ordinary
20M instruction bound; one full-array result exceeded the 1 MiB effect-value
bound; a helper closure violated cell-capture policy; whole-number arguments
arrived as floats; chat.reply required string rather than object. The final
fixture uses four bounded distinct chunks, direct calls and JSON text. No
bound, policy, expected law result or product implementation was changed.
The initial browser wait used networkidle despite SSE; it was corrected to
domcontentloaded. Failed receipts remain under `failed-*-record*`.

PostgreSQL browser certification is dependency-owned by `rb-new-pg-work`.
That lane reported three exact PG companions passed (3/3) separately; this
SQLite scorecard does not certify its PG row. The matrix declares both stores.
