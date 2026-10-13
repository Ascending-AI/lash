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

### Heap profiling

The opt-in `dhat-heap` build profiles each re-executed Python worker from
bootstrap through its first clean reset. For a receipt at `replay-receipt.json`,
profiles land at
`replay-receipt.worker-heaps/py-codemode/vm-worker-<pid>.dhat.json`, with a
matching `.receipt.json` recording the process and window. PID names keep
workers' profiles separate; a worker killed before its reset promises no
profile. The shipped TypeScript helper is profiled only when explicitly
built with `dhat-heap` and given `--heap-profile-dir`.

DHAT measurements cover host and worker heaps in separate process profiles,
never a combined heap. Host profiles from the harness's explicit DHAT
windows cover only host Rust allocations; worker profiles cover the kernel
VM and its worker embedding. Replay's counted allocation metrics remain
host-only even in a `dhat-heap` build.

Profiled workers retain confinement. The feature adds `fstat` alongside the
existing profile file calls and a read-only
`prlimit64(0, RLIMIT_STACK, NULL, ...)` query that stacker's `pthread_getattr_np`
probe needs after opening `/proc/self/maps`.
Default builds retain their original allowlist. A compile-time guard rejects
`dhat-heap` in stamped release packages; optimized `0.0.0-dev` profiling
builds remain available.

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
comparison checks: commit, dirty flag, build (`debug` or `release`), build profile,
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

Counted metrics compare across hosts within the same build, build profile
and allocator. The counted gate refuses missing or mismatched identity before
comparing any metrics, including SQL-only populations. Allocation counts differ
between `debug` and `release` builds. Timed metrics (`*_ms`) compare only within one
host and build and are never gated.

## Code-mode control (optional)

```sh
. ./env.sh
just perf-code-mode-control                         # N=1,8,32; five samples per leg
just perf-code-mode-control --n 8,64 --repeats 10
```

This builds the optimized harness and TypeScript worker through Kiln, then
runs under the 8 GiB virtual-memory cap with `/usr/bin/time -v`. The text table,
JSON receipt, and peak RSS land in `target/perf-code-mode-control/`;
`LASH_CONTROL_ARTIFACT_DIR` moves them. Each invocation uses new file stores
with **WAL and synchronous=FULL**, standard durable settings (recorded in the
header), and `Once` fixture executions. No network, merge budget or timing gate
is involved. The underlying subcommand is `lash-perf code-mode-control --n
1,8,32 --repeats 5 --store-dir <new-directory> --out <receipt.json>`.

Every leg executes the same N `fixture_lookup` calls with keys `item-0` through
`item-(N-1)`. The runner refuses a sample unless the observed tool names and
inputs match exactly, including across an owner crash. Plain sequential sends
one call per scripted model response; plain parallel sends all N in one
response. TypeScript uses sequential awaits or `Promise.all`, Python uses
sequential awaits or `asyncio.gather`. Plain legs finish with one final text
response; cells finish through `control.finish`/`control_finish`. Model-call
counts make that orchestration difference explicit.

The resume leg SIGKILLs its owner and workers synchronously after k=floor(N/2)
lookup outcomes have committed under `round.outcome`, before delivery returns
to the cell. For N=1, k=0: it cuts just before the first cell admission,
including `model.done+cell.snapshot+admit`. The model response is not yet
committed at that fused boundary, so the resumed owner regenerates it with
one Repeatable model call. Positive prefixes make no additional model calls.
That admission writes `x_start` as well, so cutting after it would interrupt
a `Once` call whose body had not yet run. A new process opens the same database and attaches to the
original send id; the engine reaps the dead node and resumes the stored turn.
There is no graceful drain, host-driven turn, or worker-only crash substitution.
Resume latency runs from fresh-owner initialization (before store open and
pool/core setup) to the next tool body's entry. The receipt keeps the bytes
returned by durable SQL calls, graph nodes/bytes hydrated by that point, and
the combined sample and its raw prefix (subtract prefix counters to recover
the resume-only window); separate owner sample JSON files remain beside each
database. Exact input equality rejects repeated
completed tools. Hydration measures restored graph work, not replayed source
instructions or all filesystem bytes.

Wall and CPU columns show median/p90 over the stated repeat count (nearest-rank
p90, no warmups). Fresh windows bracket send through outcome after session
creation; the resume row sums prefix and fresh-owner-through-outcome windows,
excluding the dead-owner/reexec gap. CPU sums the nanosecond host process clock
and the pool's charged worker CPU; raw host and worker values remain separate.
Fresh workers' initialization can be charged by their first cell execution;
these are independent-session samples, not warm persistent-session throughput.

**Logical owner commits** count successful, fenced observer receipts by label;
mailbox and maintenance writes are not owner commits. **Physical syncs** are
`null`/`n/a`: the existing observers have no fsync/fdatasync counter. WAL frames
use the SQLite witness's cache-write page count; bytes are frames × (page size
+ 24-byte frame header), excluding WAL headers and checkpoint I/O. Neither
column estimates syncs. SQL statements and rows read/written use the existing
process witness across all store connections. `staged_bytes` counts graph node
bytes prepared for owner commit attempts, including restaged nodes and the
uncommitted zero-prefix attempt; it is distinct from
physical WAL bytes. `bytes_read` counts returned scalar SQL payload in the
decorated durable port, including writes' returned values, and excludes catalog
reads and OS read amplification. The receipt records commit, dirty status,
executable SHA-256, compiler/profile, N, repeats, raw label counts and all
samples. Compare timed results only within a host/build; counted differences
explain whether batching saves model rounds, owner commits or storage work.

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
- the receipt or baseline has missing or mismatched build, build profile or
  allocator identity;
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
records `blessed_from`, build profile and allocator. Blessing refuses missing
identity; an optimized evaluator refuses the committed debug baseline.

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

The control receipt also retains `staged_commits` (canonical node counts and
bytes by kind and commit label), `commit_sql` (successful owner SQL windows,
including newly installed canonical bytes and bounded statement shapes),
`durable_calls.<method>`, host allocator deltas, and directional IPC message
and byte counts. SQL bound bytes are expanded SQL spelling, not disk bytes;
WAL and installed canonical bytes retain their separate units. Allocation
deltas are process-wide and include the observer and other host threads.

For runtime-perf, `LASH_PERF_PHASE_CLOCK_ONLY=1` keeps the phase callbacks and
clocks while omitting phase allocator and RSS samples. Phase allocation fields
are zero and RSS growth is unknown (`null`); outer stage allocation and memory
samples remain live. The report records `phase_observation_mode`; clock-only
observation refuses allocation-budget enforcement because phase allocations
were not sampled. `run_turn` closes before the validation state export.
Separate profiling runs can set `LASH_PERF_CONTROL` and `LASH_PERF_ACK` to
perf-stat's control and acknowledgement FIFOs (`--delay=-1 --control=fifo:...`).
The runtime turn future or control send/outcome window enables and disables
counters with acknowledgements; setup, final validation and reporting are
outside those instruction windows. Do not use these profiling runs as the
unprofiled latency population. `LASH_PERF_COMMIT_TRACE=sqlite` emits optional
stderr commit markers and runtime-perf owner SQL receipts for syscall
attribution; `postgres` selects PostgreSQL receipt units.

Durable runtime-perf turns also report `turn_work.*` SQL, WAL, graph-encoding
and hashing totals across their send/settlement windows. These counters exclude
construction, seeding and validation; they include all store connections and
concurrent maintenance in the window. The existing `runtime_work.*` counters
retain their broader whole-scenario scope.
