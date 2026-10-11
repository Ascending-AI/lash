# Replay workloads

```sh
just perf-replay                                  # the whole corpus, SQLite file stores
just perf-replay chat-short ts-codemode --turns 20
just perf-replay --store postgres                 # needs LASH_POSTGRES_DATABASE_URL
kiln test //crates/lash-perf:replay__test         # the per-merge counted gate
just perf-replay-bless                            # re-bless the gate's baseline
```

`just perf-replay` writes `target/perf-replay/replay-receipt.json`
(`LASH_REPLAY_ARTIFACT_DIR` moves it) and prints one table per workload. On a
shared host, wrap the binary in `(ulimit -v 8388608; /usr/bin/time -v ...)`.

## What runs

Each workload is a committed script in `crates/lash-perf/workloads/replay/`
(format `lash-perf-replay/1`, one file per workload). The runner creates
live sessions on a core that serves its own node over a real store and,
turn by turn, calls `send()` then `outcome()`. The provider is scripted and
stateless: a marker at the start of each user input names the turn, and the
step is the number of assistant messages after it, so a resumed turn gets the
same reply. Code-mode cells run in real worker processes: TypeScript on the
shipped `lash-vm-worker` (`LASH_VM_WORKER`), Python on `lash-perf` itself
re-executed with the Python dialect installed. Tools are deterministic
fixtures with no network:

| Tool | Answers |
|---|---|
| `fixture_lookup {key}` | a small record derived from `key`; takes extra arguments |
| `fixture_large {key, bytes}` | `bytes` of text derived from `key` (incompressible) |
| `fixture_slow {key, ms, kill_workers?}` | after `ms`; optionally SIGKILLs every code-mode worker first |

A turn must make exactly its scripted model calls. A cell that fails, a tool
that is refused or a turn that ends early stops the run and quotes the
request that left the script, so a broken script never measures silently.

| Workload | Shape |
|---|---|
| `chat-short` (W1) | 20 standard turns, each a native lookup then a text answer |
| `chat-long-one-frame` (W2) | 600 plain-text turns, never compacted (the latency gate's shape) |
| `chat-long-compacting` (W3) | W1's turn for 600 turns, compacting every 50 |
| `ts-codemode` (W4) | 50 TypeScript turns: turn 1 keeps a 1 MB value in a global and saves a function; each later turn awaits two tools (each await parks the cell) with it live |
| `py-codemode` (W5) | W4 in Python, reading tool replies as dicts |
| `concurrent-16` (W6) | 16 sessions of W1 on one store, in lockstep rounds |
| `crash-resume` (W7) | W4's setup, then 10 turns; every third kills every worker while the cell is parked, and it resumes on a fresh worker (three crashes stay under the pool's restart-storm bound) |

## Reading a receipt

The receipt (`lash-perf-replay-receipt/1`) carries the identity every
comparison checks: commit, dirty flag, build (`debug` or `release`),
allocator, host, CPU count and 1-minute load at start. Each workload records
its script digest (SHA-256), backend, sessions and turns.

`rounds[*].metrics` are differences of cumulative counters read before a
round's first send and after its last outcome, summed over the round's
sessions. The runner holds periodic artifact-cleanup passes at their admission
boundary and drains due cleanup through the real relay between rounds. These
maintenance passes are outside the turn window; immediate producer cleanup
still belongs to its turn. This gives each round the same settled artifact
referrers, independent of the recovery worker's ten-second cadence. Summaries
divide by sessions, so every statistic is per session turn:

| Metric | Unit | Counted by |
|---|---|---|
| `sql_statements`, `sql_selects`, `sql_writes`, `sql_transactions` | statements | the SQLite perf witness (every store connection's statement trace) |
| `sql_rows_read` | rows | result rows the trace saw |
| `sql_rows_written` | rows | SQLite `total_changes` across each connection call |
| `store_bytes_written` | bytes | pages written to the WAL (`SQLITE_DBSTATUS_CACHE_WRITE`) times the page size |
| `graph_nodes_written` | nodes | net growth of `graph_nodes` rows, read on a side connection |
| `graph_nodes_decoded` | nodes | the store's `decoded_graph_node_bodies` counter |
| `allocations`, `allocated_bytes` | count, bytes | the host's `stats_alloc` allocator (not the workers') |
| `ipc_frames`, `ipc_bytes` | frames, bytes | the worker pool's measurements, both directions |
| `worker_executions`, `workers_killed` | count | the pool; the crash fixture |
| `model_calls` | calls | the scripted provider |
| `host_cpu_ms`, `worker_cpu_ms`, `wall_ms` | ms | `/proc/self/stat`; the pool's charged CPU; the round's wall clock |

A metric the backend cannot count is absent, never zero. PostgreSQL rounds
carry no SQL, WAL or decode counts (its stores count none of them per
process); they keep allocation, CPU, IPC, node and wall metrics.

Per metric, the summary gives the total over every round and, over the
steady (non-setup) rounds, the per-turn median, p95 and max, and four
slopes in the metric's unit per prior turn: least squares and Theil-Sen
(median pairwise) against history depth, and the same two against the
frame's depth (turns since the last compaction; equal to history depth for a
workload that never compacts). A flat cost has slope 0. O(history)
work shows as a positive history slope; O(frame) work in a compacting
session as a frame slope with a near-zero history slope. Theil-Sen ignores
the odd sweep or retention turn that moves least squares.

Counted metrics compare across hosts. Allocation counts differ between
`debug` and `release` builds. Timed metrics (`*_ms`) compare only within one
host and build and are never gated.

## The per-merge gate

`//crates/lash-perf:replay__test` runs `lash-perf replay-gate`: the subset in
`workloads/replay/budgets.json` (`chat-short`, `chat-long-compacting` at 200
turns, `ts-codemode` and `py-codemode` at 12) on SQLite file stores, in the
test's debug build. It fails when:

- a counted budget in `budgets.json` is exceeded: a per-turn median or max
  ceiling, or a depth-slope ceiling (`history_slope`, or `frame_slope` for
  the compacting workload);
- a counted per-turn median rises more than `baseline_tolerance.relative`
  (plus any per-metric `absolute` slack) above `baseline.json`;
- a depth slope listed under `baseline_tolerance.depth_slope` rises more than
  `relative * |baseline| + absolute` above its blessed value; a
  `<workload>/<metric>` key loosens one noisier workload;
- a gate workload did not run, or its baseline names another script digest,
  backend or length.

Wall time is never asserted.

`known_breaches` lists budgets main exceeds today, each with the ticket that
owns the fix. A known breach is reported, not failed, while it still breaches;
when its owner's fix lands and it stops breaching, the gate fails until the
entry is deleted, so the budget then binds. The depth-slope ratchet still
applies to a known breach: on 2026-10-11, re-introducing the FIG-5501
terminal-window copy on a tree blessed without it failed the gate on the
`allocations` and `allocated_bytes` depth slopes of `chat-short`,
`chat-long-compacting` and `ts-codemode`.

### Re-blessing

When a change moves the baseline on purpose, run `just perf-replay-bless`
and commit the rewritten `workloads/replay/baseline.json` with the change,
stating why in the commit body. The gate compiles `budgets.json` and
`baseline.json` in, so a rebuilt test reads the new baseline. The gate prints `improved ...` lines when a
median or slope falls below its tolerance band; re-bless then to ratchet the
gain. Re-bless only from a clean tree at the change's commit: the baseline
records `blessed_from`.

## Main's trend

`lash-perf replay --history <file.jsonl>` appends one duration-history record
per workload, profile `replay`, scenario `replay/<workload>`. The record holds
the per-turn wall median and p95, `turn_host_cpu_ms` and `turn_worker_cpu_ms`,
and every counted metric's per-turn median and robust history slope in its own
unit. The file is then read with the advisory detector of
[duration history](duration-history.md). The Performance workflow records it
into `.benchmarks/perf-history/replay.jsonl` beside the runtime and kernel
series; only main saves that cache. A rolling main job (for example the
`lash.tests_main` hook) records the same series with:

```sh
just perf-replay --history <persistent>/replay.jsonl
```

It needs an optimized build, a persistent history path and about 15 minutes.

## Adding a workload

Write `workloads/replay/<name>.json`:

```json
{
  "format": "lash-perf-replay/1",
  "name": "<name>",
  "description": "What it models and why.",
  "protocol": "standard",
  "sessions": 1,
  "compact_every": 50,
  "turns": [
    { "repeat": 20, "user": "Turn {turn}: ...",
      "replies": [
        { "tool_calls": [{ "tool": "fixture_lookup", "args": { "key": "item-{turn}" } }] },
        { "text": "Answer {turn}." }
      ] }
  ]
}
```

A group with `"setup": true` (a code-mode session loading its state) runs
and is recorded, but its turns stay out of the per-turn medians and slopes;
totals still count them. `protocol` is `standard`, `typescript` or `python`. Code-mode replies are
`{ "cell": ["line", ...] }` and should end with `await control.finish(...)`
(`control_finish` and `tools_<name>` in Python). `{turn}` and `{session}`
expand everywhere. Add the file to `CORPUS` in `src/replay/script.rs` and to
this page's table. To gate it, add it to `subset` and its budgets to
`budgets.json`, then re-bless.

### Recording a real session

```sh
kiln run //crates/lash-perf:lash-perf__bin -- replay-record \
  --db <host or workbench>/lash.db --session <session id> \
  --name <new-name> --protocol standard --out crates/lash-perf/workloads/replay/<new-name>.json
```

The recorder reads the session's committed transcript and keeps its shape,
not its content. Every user and assistant text becomes deterministic filler
of the same byte length. A native tool call becomes a `fixture_large` call
answering as many bytes as the real result. A cell keeps its source, since it
must still run, with e-mail-, key- and long-digit-shaped tokens replaced by
`x` and its `tools.<name>(` calls pointed at `fixture_lookup`. A recorded
cell that reads fields of a real tool's result needs a hand edit before it
replays. Read the script before committing it.
