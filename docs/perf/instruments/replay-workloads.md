# Replay workloads

```sh
just perf-replay                                  # the whole corpus, SQLite file stores
just perf-replay chat-short ts-codemode --turns 20
just perf-replay --store postgres                 # needs LASH_POSTGRES_DATABASE_URL
kiln test //crates/lash-perf:replay__test         # the per-merge counted gate
just perf-replay-bless                            # re-bless the gate's baseline
```

`just perf-replay` builds with `--config=optimized` and writes `target/perf-replay/replay-receipt.json`
(`LASH_REPLAY_ARTIFACT_DIR` moves it) and prints one table per workload. On a
shared host, the recipe caps the replay at 8 GiB of virtual memory and records
`peak-rss.txt` with `/usr/bin/time -v`. The counted baseline blessing recipe
continues to use the gate’s debug build.

## What runs

For a deployment comparison, a replay script may set `memory_limit_mib` to
the deployment's logical heap, within 1..=512 MiB. The default is 512 MiB;
the receipt records the selected value as `cell_memory_mib`. Keep this
setting identical across a performance pair. For example, Figments' 64 MiB
cell budget can take a different execution path from the default profiler.

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

### Figments record build and comparator sort

```sh
just perf-replay ts-record-sort-200k --sql-shapes .kiln/hostflows/records-sql.json
```

This optimized SQLite-file workload builds the full 200,000 records
`{id: i, key: (i * 7919) % 200000}`, then sorts that same retained array
with `(a, b) => a.key - b.key`. The printed window table keeps turn 1
(build) and turn 7 (sort) separate. Turns 2–6 and 8–12 transfer all keys in
five consecutive 40,000-key chunks after each compute cell, and are explicitly
excluded from steady summaries. Their transport and checkpoint cost belongs
to those verification windows; the Rust oracle runs after each counter cut.
The ordinary window includes the cell's durable execution and checkpoint;
it is not a timer around only the JavaScript statement. This full population
explicitly admits 1,000,000,000 guest instructions, with the matching worker
ceiling recorded in its geometry; the ordinary 100,000,000-instruction budget
refuses the full comparator sort. Other workloads retain their existing bounds.

Verification cells return the record count and actual numeric keys in bounded
chunks, staying below the existing 1 MiB effect-value bound. Rust independently
generates the expected integer keys and uses Rust's integer sort for the ordered
oracle. Every run checks count, first/middle/last keys, the complete sequence,
and sortedness, including every chunk boundary. The receipt retains verified
record count, chunk range and key samples on each verification turn. At full
size the sorted population's first/middle/last keys are 0, 100000 and 199999.
`--turns` cannot truncate an oracle workload.

This population is for scheduled and manual measurement. It does not alter
the counted subset or its baseline. Wall time on shared hosts is noise-affected;
no wall-time number gates a merge. The SQL sidecar uses these same turn cuts
and retains shape × store-origin attribution.

### Host catalogues with per-turn authority

```sh
just profile-turn hostturn --turn 3
# The existing namespace lifecycle, with the same host catalogue plus tools.mark:
python3 scripts/profile_runtime.py --boundary workflow-namespace --release \
  --boundary-arg=--workload=hostturn --boundary-arg=--operations=1 \
  --out .kiln/hostturn-namespace.json
```

`hostturn` supplies 31 structured tools across 11 package roots, owner options
on every send, and a fresh capability binding with per-tool `turn_id` values.
It warms one TypeScript RLM turn, then runs three quiet text turns with no
cells or tool calls. Quiet turns assert zero snapshot, manifest and link-payload
callbacks. `host.tools`, `host.schema_fields` and `host.package_roots` vary the
catalogue independently of the turn's work. Prompt limits are 128 KiB per
section and 512 KiB total.

`just profile-turn host-document --turn 3` uses the same catalogue for a
first text turn, a quiet warm turn and a host document edit. The third turn
reads `tools.host0` and must finish with the independently specified edited
text. Its tool binding changes revision while the other operations stay the
same. This models a host document store and tool grants, rather than a raw
Lash message attachment. The scripted provider injects zero delay; the
document store is in memory and the cell finishes directly, without the
production provider's follow-up response. PostgreSQL requires `kiln gate`.

The physical profile records `environment collection` around source callbacks.
It excludes projection, package retention and checkpoint work, which remain in
their existing parent intervals. `provider dispatch` marks entry into the scripted
provider. Its first interval's `start_ns` gives elapsed time from
the turn-profile start to actual provider entry, including earlier admission,
preparation and checkpoint work. It is distinct from the broader `model call`
phase, which also includes Lash journal work. The scripted provider injects no
network latency, and its dispatch interval does not measure production TTFT.

The root/provider, owner-option, authority and prompt-limit geometry follows
Figments revision `083899321`, specifically `apps/lash-runtime/src/tools.rs`,
`crates/figments-lash-runtime-api/src/turn_run.rs`, `run_authority.rs`, `lib.rs`
and `prompt.rs`. Schemas and tool distribution are synthetic. This workload
reproduces those mechanisms, without claiming a byte-identical production
session. Compare matching profiles and backends. The earlier FIG-7453 source
used a debug Figments deployment; FIG-7482's repin-37 uses an optimized release
build. Phase wall durations on a shared host are observations,
not latency guarantees.

### Warm host capabilities and a fresh document

```sh
LASH_REPLAY_ARTIFACT_DIR=/tmp/warm-caps just perf-replay warm-capabilities \
  --sql-shapes /tmp/warm-caps/sql.json
```

This versioned workload runs on a SQLite file store and prints a row for each
turn. Turn 1 primes 31 resident host tools; turn 2 is a quiet clean model turn
carrying unchanged `RunSpec.capabilities`; turn 3 stores and attaches a fresh text
document and publishes the host's new document operation. Every tool body checks
its exact per-turn document capability. Setup and fresh-document turns must each
execute exactly one authorized operation; the quiet turn must execute zero.
Native tool invocation also performs legitimate by-ID contract lookups, so the
quiet model window excludes that separate work. Turn 2 must enter zero snapshot, manifest and link_payload callbacks;
turn 3 must enter some. The snapshot adapter uses the existing callback counters
and observes the default snapshot's calls into the actual runner separately.

The receipt joins callback entries to `sql_writes`, `sql_rows_written` and
`store_bytes_written` using the same settled before/after cuts. Callback entries
are not writes; SQL write statements, changed rows and WAL bytes are distinct
units. SQL shape/origin attribution is retained in the usual `--sql-shapes`
receipt. Wall time on a shared host is noise-affected and never a gate.

### SQL shape attribution and execution joins

```sh
. ./env.sh
just perf-replay crates/lash-perf/workloads/replay/scenarios/coding-typical.json \
  --sql-shapes .kiln/FIG-6400/coding-sql-shapes.json
```

The underlying command is `lash-perf replay <scenario> --store-dir <stores>
--out <replay.json> --sql-shapes <shapes.json>`. Select exactly one workload.
The sidecar tags `rounds` entries with `phase`: session start (turn zero), each ordinary turn, and compactions, with normalized SQL
`shape` × `store_origin` rows, `statements`, `selects` and `rows_returned`.
Each turn’s sums are checked against the existing replay metrics using the
same locked SQL snapshot; export refuses a mismatch. The receipt records
`conserved: true`, exact totals, script digest and build identity. Values and
comments are redacted using the existing SQL shape normalizer. `selects` uses
the meter’s SELECT/WITH bucket; returned rows include PRAGMA and RETURNING
results, and count neither scanned rows nor network trips.

Store origins are the types of the closures actually executing on SQLite’s
connection worker. Tags survive delegated reads, batched writes and nested
durable adapters. `connection/batch envelope` explicitly names work outside
a member body, such as fences. This is process-wide store attribution,
including overlapping engine/background tasks; it does not partition higher
async callers of the same method. Statements count at PROFILE and rows at ROW,
so an execution spanning a snapshot cut can contribute rows and completion to
different turns, exactly as the existing totals do.

The [joined execution receipt](sql-operation-windows.md#turn-sql-work-joined-execution-receipts-fig-6621) also supports PostgreSQL, session start, compaction, maintenance and shutdown.

The observer retains at most 4096 shape/origin pairs per workload and refuses
an incomplete export on overflow. Aggregation happens in memory; there is no
per-statement logging. Without `--sql-shapes`, there is no histogram, origin
TLS access, normalization or attribution lock on store calls; ordinary production builds
compile the witness out. Enabled allocation/timing numbers include the
observer and must not replace unobserved baselines. A matched on/off
observation is recorded below.

A 2026-10-15 observation on `7a5ccc1d7897` plus this instrumentation used
fresh coding-typical stores, five turns and 22 model calls per mode, one
symbolized opt-level-3 `stats_alloc` executable, SQLite NORMAL. Existing replay
counters remain active in both modes; this pair measures extra attribution.

| SQL attribution | SELECT median | Returned-row median | Turn wall median | Host allocated-byte median | Peak RSS |
|---|---:|---:|---:|---:|---:|
| off | 512 | 705 | 75.484 ms | 15,049,805 B | 101,300 KiB |
| on | 512 | 705 | 72.448 ms | 15,187,099 B | 101,800 KiB |

All five enabled turns conserved statements, SELECTs and returned rows
exactly; the largest turn retained 155 shape/origin pairs. Observed on/off
ratios were 0.960× for median turn wall and 0.899× for invocation wall
(0.683→0.614 s); allocation increased by 137,294 B/turn at the median.
This is one ordered pair on a shared host: timing variation prevents a
calibrated overhead estimate. The 512-SELECT median reproduces the earlier
515-SELECT observation within the cohort's normal scheduling variation.

### Heap profiling

```sh
. ./env.sh
python3 scripts/profile_heap.py --population replay \
  --workload crates/lash-perf/workloads/replay/scenarios/coding-typical.json \
  --turns 5 --profile-turn 4 --out-dir .kiln/FIG-6400/coding-heap
python3 scripts/perfreport.py .kiln/FIG-6400/coding-heap/dhat/parent.dhat.json --top 10
```

This builds the opt-level-3, symbolized profiling platform through Kiln and
runs one ordered pair with the **same DHAT executable**, profiler inactive
then active. Both replays use fresh SQLite stores and validate all scripted
model calls. `receipt.json` records the build report, executable hashes,
allocator, identities, actual selected turns, per-turn wall observations,
profile/window receipt and per-process `/usr/bin/time -v` resource files.
Each replay is capped at 8 GiB virtual memory. Runtime stores live under
`target/heap-replay/` and are removed after success. Reuse a build with
`--no-build --build-report <previous-build.json>`.

`--profile-turn K` is one-based. Optionally add `--profile-through-turn L`
for a contiguous inclusive range; ranges crossing compaction are refused.
The window begins before the first selected turn’s meter read and ends after
the last selected outcome and meter read. Session creation, earlier turns,
compaction, report writing and shutdown are excluded. A range includes inter-turn cleanup and record assembly between its endpoints.
Already-live history
is excluded unless reallocated in the window. Direct replay accepts the same
turn flags with `--dhat-out <path>` and optional `--dhat-frames N` on a
`dhat-heap` build. The launcher uses 64 frames to reach application callers.
`perfreport.py` resolves the first application source frame, including unique
shortened workspace paths, and retains file:line; allocator filtering uses
source paths, never optimized function-name substrings.

Compare turn 1 and turn 4 with separate fresh output directories to attribute
history growth at equal tool count. DHAT traffic (`tb`), simultaneous-peak
live bytes (`gb`), site maximum (`mb`) and window-end live bytes (`eb`) are
different quantities. Site maxima cannot be summed as a simultaneous peak.
These are host Rust allocations, excluding workers and SQLite’s native
allocator. DHAT bookkeeping changes allocation counts, time and RSS; the
ordered off/on pair is a shared-host diagnostic, not a calibrated overhead
estimate or a timing gate.

On the same source, coding-typical turn 1 and turn 4 captures each completed
five turns/22 model calls per mode, with one selected turn profiled. DHAT
send→outcome wall ratios were 10.34× and 11.94×, and peak RSS was
949,884/948,492 KiB versus 102,140/102,908 KiB with the host profiler inactive.
All runs used the 8 GiB cap. Prompt-admission stacks through
`prompt_sections.rs:621`/`:546` and `composer.rs:231`/`:245` grew from
542,315 to 1,155,928 requested bytes, or 204,538 B/prior turn over this short
span. The scripted provider grew separately by 391,862 B/prior turn.
First-use work in the remaining stacks makes the all-stack secant smaller;
these typical-size, first/fourth-turn samples do not reproduce the historical
heavy-size 4→24-turn allocation secant. The source attribution matches that
report's prompt construction/decoding finding. The captured turn-4 prompt
family had zero window-end live bytes. Grouping each stack by its first
application source file ranks `prompt_sections.rs` first for growth:
361,253→997,953 B, or 212,233 B/prior turn. These quantities describe traffic
and the selected window, not total retained session memory.

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
divide each round total by sessions. These round-average observations are
not individual session-turn latency samples:

| Metric | Unit | Counted by |
|---|---|---|
| `sql_statements`, `sql_selects`, `sql_writes`, `sql_transactions` | statements | the SQLite perf witness (every store connection's statement trace) |
| `sql_rows_read` | rows | result rows the trace saw |
| `sql_rows_written` | rows | SQLite `total_changes` across each connection call |
| `sql_bytes_written` | bytes | the values bound to statements that write: a text's or blob's own bytes, eight per number |
| `store_bytes_written` | bytes | pages written to the WAL (`SQLITE_DBSTATUS_CACHE_WRITE`) times the page size |
| `graph_nodes_written` | nodes | net growth of `graph_nodes` rows, read on a side connection |
| `graph_nodes_decoded` | nodes | the store's `decoded_graph_node_bodies` counter |
| `allocations`, `allocated_bytes` | count, bytes | the host's `stats_alloc` allocator (not the workers') |
| `ipc_frames`, `ipc_bytes` | frames, bytes | the worker pool's measurements, both directions |
| `worker_executions`, `workers_killed` | count | the pool; the crash fixture |
| `model_calls` | calls | the scripted provider |
| `host_cpu_ms`, `worker_cpu_ms`, `wall_ms` | ms | `/proc/self/stat`; the pool's charged CPU; the round's wall clock |

`store_bytes_written` is the physical cost, and it moves by a few percent
between runs of one commit: rows keyed by content hashes that hold run ids
and timestamps land on different B-tree pages each run, so the same writes
dirty a different number of pages. `sql_bytes_written` counts what the
store asked to write and gives the same value every run (to a few bytes of
id and timestamp spelling), so the gate's byte-slope budgets bind it
(FIG-6280).

The Python gate runs 24 turns (FIG-6751). Its canonical prompt-prefix tree
fills its first levels during the initial rounds: the 12-turn row-slope
estimate overweights those few steps. In the attributed 24-turn observation,
kernel candidate writes stay at 269 rows per steady turn, the total row slope
is 0.4, and turns 13–24 have a zero robust row slope. The ordinary observation
measures 0.375 over all steady turns. The longer window keeps the existing
0.5-row slope tolerance and every byte-slope ceiling.

FIG-6751 re-derived only Python's physical-write median as 2,453,504 B from
an ordinary 24-turn debug replay on `2d246aab61`; its historical 25% headroom was later replaced by measured absolute noise in FIG-7434
(3,066,880 B). Other baseline metrics retain their previous values. The
bisection's adjacent samples measured 2,338,816 B on `cf1c07b95a` and
2,371,584 B on `47e8cfc5bd` (FIG-6717), whose commit explicitly states:
“Page shapes change in place (dev-phase format).” Its canonical boundaries
replace overflow splits and change which kernel pages a snapshot writes.
Paired physical attribution over the same eleven steady turns totals
26,009,600 → 26,226,688 B: `cell.snapshot+admit` accounts for +270,336 B;
root leases and turn admission are unchanged. Per-kind totals include SQLite
page-placement variation, so a single record's difference is not an exact
allocation to its logical rows. This is a page-shape baseline change, not a
new write, snapshot, or slope exemption.

A metric the backend cannot count is absent, never zero. PostgreSQL rounds
carry no SQL, WAL or decode counts (its stores count none of them per
process); they keep allocation, CPU, IPC, node and wall metrics.

Per metric, the summary gives the total over every round and, over the
steady (non-setup) rounds, the per-turn median and explicitly named `round_average_p95` and
`round_average_max` diagnostics, and four
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
including `model.done+cell.snapshot+admit`. The earlier `model.answer`
commit retains the paid response, so both zero and positive prefixes resume
without another model call.
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
reads and OS read amplification. The exact useful-work check compares
root-local observed tool names (`fixture_lookup`) and arguments; model-facing
calls retain `tools.fixture_lookup`. The receipt records commit, dirty status,
executable SHA-256, compiler/profile, N, repeats, raw label counts and all
samples. Compare timed results only within a host/build; counted differences
explain whether batching saves model rounds, owner commits or storage work.

## The per-merge gate

`//crates/lash-perf:replay__test` runs `lash-perf replay-gate`: the subset in
`workloads/replay/budgets.json` (`chat-short`, `chat-long-compacting` at 200
turns, `ts-codemode` at 12 and `py-codemode` at 24) on SQLite file stores, in the
test's debug build. It fails when:

- a counted budget in `budgets.json` is exceeded: a per-turn median or max
  ceiling, or a depth-slope ceiling (`history_slope`, or `frame_slope` for
  the compacting workload);
- a counted per-turn median exceeds `baseline.json`'s `median_ceiling`: the
  maximum of five same-commit runs plus their max-minus-min absolute noise;
- a depth slope listed under `baseline_tolerance.depth_slope` rises more than
  `relative * |baseline| + absolute` above its blessed value; a
  `<workload>/<metric>` key loosens one noisier workload;
- the receipt or baseline has missing or mismatched build, build profile or
  allocator identity;
- a gate workload did not run, or its baseline names another script digest,
  backend or length.

Wall time is never asserted.

Each gate workload runs in its own fresh producer process, five times. Each
producer uses a private current-thread runtime, one blocking worker, one prompt
renderer and SQLite's synchronous request/reply option. The reply uses a fixed
allocation and a condition variable. Inline connections use one reusable worker
queue slot, so unbounded-channel block allocations cannot move between turns
when cold SQL counts change.
The fixture fixes its injected clock, send IDs, lease incarnation, environment
owner/source IDs and logical publication domain. Its frozen clock returns
stateless pending waits for future deadlines. Polling a losing timer branch
therefore neither advances time nor allocates a boxed timer state. Presentation
coalescing is off, so the scripted chunks have no wall-clock frame timer.
The single renderer also fixes cache-admission order and keeps lazy native
worker-wait initialization out of later turns. One blocking worker keeps the
VM exchange's thread-local analysis and park state on the same thread instead
of initializing another thread when the previous worker is finishing its reply.

The SQLite enqueue nonce is fixed before the fixture opens any session. Thus
compaction command ids and the frame identities they derive are fixed as well.
VM staging uses a private directory under that producer's store root, so spool
startup never scans sibling processes' live or abandoned staging directories.

Physical files remain private to each capture. The stable logical domain keeps
the original domain's byte length. The testing-only identity source is installed
once per producer and disabled when that producer's workload ends. Process exit
releases that workload's caches and detached observers before another workload
starts. Ordinary replay retains its usual execution setup.

Counted stores use local after-commit hints without cross-process wake listeners
or publisher tasks. Facade follow/terminal recovery polls, SQLite wake polling
and automatic WAL checkpoints are deferred beyond the population. Facade
observations still wake from the private node's real activity events. The
tool-live publisher uses a 60-second flush interval and one-hour completed-slot
linger; these fixtures emit no tool-progress facts. Completed slots remain
allocated through the short population instead of moving the next Vec growth
according to a real-time retirement cadence.

Each counted session subscribes to its local acknowledged-commit watch before
sending. After the exact input's terminal handoff, it reads the public outcome
once. The private single-node fixture therefore measures terminal observation
without speculative live-follower reads racing intermediate commits. Its
existing watchdog still fails a turn that never hands off a terminal.
Before the counter cut, the single-threaded fixture also waits for its local
actors to become idle, so owner cleanup after that handoff stays in the turn.

Foreground commits, worker exchanges, send/outcome observation and meter reads
remain counted process-wide. Artifact cleanup and page retirement settle
between turns. This gate measures foreground allocation growth with one async issuing thread.
Periodic maintenance is deferred; SQLite and blocking-worker allocations still
count. The watchdog and scripted
tool waits still use Tokio time. Each receipt records its runtime geometry,
clock/identity policy and SQLite settings. No allocation is sampled, subtracted
or replaced with an estimate.

Private persistent indexes, the spool's reference-count map and the checkpoint
preparation cache use deterministic hashers. Equal updates then copy equal trie
paths and grow their tables at equal points after removals. The preparation
cache still compares whole input bytes, updates LRU order on hits and obeys its
unchanged entry and byte bounds. Cold resident indexes insert in resident-node
order instead of randomized source-map iteration order. These are in-memory
allocation properties; stored identities and wire hashes do not change. Spool
paths pad the PID to ten digits, and timed CPU sampling uses a stack buffer.

The replay law requires all five raw TypeScript host-byte history slopes to
match exactly. It also compares every counted summary and every turn and
compaction series for every gate workload before accepting the budget verdict. The existing median
and worst-of-five gate statistics are unchanged. Sibling `.0.json` through
`.4.json` files retain the five complete cohorts; `.0.0.json` through `.4.3.json`
retain the individual workload producers and their identities.

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
`baseline.json` in, so a rebuilt test reads the new baseline.

The writer never raises an existing ceiling. If every sampled maximum fits an
existing ceiling but a wider sampled noise range would raise it, the writer
retains that metric's complete previous calibration and tightens the other
metrics. A sampled maximum above its existing ceiling still refuses the refresh.

The gate prints `improved ...` lines when a
median or slope falls below its tolerance band; re-bless then to ratchet the
gain. Re-bless only from a clean tree at the change's commit: the baseline
records `blessed_from`, build profile and allocator. Blessing refuses missing
identity; an optimized evaluator refuses the committed debug baseline.

### Configuration graph baseline (FIG-6637)

The 2026-10-17 blessing attributes the additional read rows and chat
allocations to `fd0652f96d` (FIG-6599). Adjacent-commit runs of
`the_replay_corpus_subset_stays_within_its_counted_budgets` measured:

| Metric / workload | Parent `1fc915f4c8` | Graph commit `fd0652f96d` |
|---|---:|---:|
| Rows read / chat-short | 256 | 750.5 |
| Rows read / chat-long-compacting | 286 | 810.5 |
| Rows read / ts-codemode | 551 | 1098 |
| Rows read / py-codemode | 540 | 1111 |
| Allocated bytes / chat-short | 4,930,152 | 6,885,002.5 |
| Allocations / chat-short | 27,618 | 35,277.5 |
| SELECTs / chat-short | 238.5 | 344.5 |

FIG-6599 requires field-addressed immutable configuration shared by revisions
and recorded runs. Head and terminal-window queries now return `config_ref`
instead of joining the complete JSON body. Loading that reference validates
its graph closure and reconstructs the same recorded configuration without
calling producers. The chat trace has 45 configuration rows per closure,
read through history windows, head reads, admission and commit. Eleven ordinary
closure reads account for about 495 additional returned rows per turn. All
scripted model-call counts and successful turn outcomes were validated on
both sides of the commit.

Run `just perf-replay-bless` without SQL instrumentation, then retain only
these six attributed medians: the four workloads' `sql_rows_read`, plus
chat-short's `allocated_bytes` and `allocations`. The committed blessing is
from clean `8d31251a7f`; it preserves every other median and every depth slope,
including Python's still-breaching write slope. FIG-6617 reduced the SELECT
cost; the unobserved blessing measured 335 chat-short SELECTs against the
existing tolerance ceiling of 355.625, so its SELECT baseline stays unchanged.
The row-read history ceiling stays at 2. The reported 2.1 slope did not
reproduce at `6b32e646b0` (1.0); the unobserved blessing measured 1.422.
Head-read counts can vary with asynchronous activity and counter cuts; this
blessing does not certify those counts as deterministic or change their guard.

## Main's trend

`lash-perf replay --history <file.jsonl>` appends one duration-history record
per workload, profile `replay`, scenario `replay/<workload>`. The record holds
the per-turn wall median and p95, `turn_host_cpu_ms` and `turn_worker_cpu_ms`,
and every counted metric's per-turn median and robust history slope in its own
unit. The file is then read with the advisory detector of
[duration history](duration-history.md). The manually dispatched Performance
workflow appends `.benchmarks/perf-history/replay.jsonl` beside the runtime
and kernel series and saves that cache on main. That wiring alone does not
establish a scheduled producer or an owner reviewing findings. A scheduled
owner can supply a persistent path and run:

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

`--protocol` is the session's own protocol: the recorder reads the session's
committed transcript through that protocol's decoder, so a code-mode session
yields the cells it committed. It keeps the transcript's shape, not its
content. A turn is one turn identity; inputs a turn admitted together become
one user input as long as their texts joined by newlines. Its replies are the
model's: a text, a native tool batch or a cell, each cell once whatever
companion context its channel committed. Reasoning alone and the reply a
cell's `finish` sealed are not replies. A tool result sizes the call its id
names, and a call no result answers refuses the capture, as does a code-mode
reply whose native tool calls ran no cell. Every user and assistant text becomes deterministic filler
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


Replay summaries report `steady_rounds`, `distinct_history_depths` and
`distinct_frame_depths`. Fewer than two distinct depths give null slopes
(`unknown` in the text table), while measured flat work retains a numeric zero.
A slope budget refuses an unknown slope; its existing limit is unchanged.
The maximum budget is named `round_average_max` and keeps the same predicate.

```sh
kiln test //crates/lash-perf:lash-perf__unit_test \
  --test_arg=replay::receipt::tests::insufficient_depths_are_unknown_not_flat --test_arg=--exact
```

Receipt identity carries `sql_shapes`, `traffic` and `heap_window`; each workload's
`experiment` retains the actual backend settings, heap frame depth and worker artifact before
timing. The same identity accompanies SQL-shape and heap sidecars and history.

The counted baseline is the ordinary replay population. SQL-shape, traffic or
heap observers receive a typed observer-identity refusal instead of entering
that population; blessing also refuses those modes. No budget is relaxed.

The text reader prints full producer/workload identities and each metric's
steady-round and distinct-depth populations alongside round-average median,
p95 and maximum diagnostics.

### Replay full-run reconciliation (FIG-7499)

The full-run triage reproduced stale corpus identities, ready-worker cold
allocation, a restored tool/sleep race, workflow coverage/path disagreement
and the counted replay failure on `613172eefb49`. The original laws and all
strict replay determinism assertions remain intact.

The replay cut waits for local owner idle and complete renderer capture
disposal. Inline SQLite replies wait for disposal of their command and its
captures. Compaction readback follows a new owner-idle generation after
command admission, so polling cannot move four SQL statements and a
transaction between records. Attachment write tokens retain their random
identity and 32 lowercase hexadecimal digits; UUID's stack encoder avoids
the leading-zero formatter's random allocation calls. That formatter came
from `a2f2938d5e0`. Cold fenced-read recovery introduced in `c166d5df16` is
boxed on the error branch, keeping its state out of successful read futures.

The TypeScript parser uses one reusable mutex/condition-variable
request/response slot. Its previous rendezvous response channel, introduced
in `ce55f7e26da`, allocated a four-entry waiter vector only when a receive
first blocked. Comparing every 96-byte allocation stack in the affected
worker interval found exactly one added stack in the higher sample, through
`std::sync::mpmc::zero::Channel::recv` and `Parser::parse`. Normal parser
allocations matched. The slot removes that scheduling-dependent allocation
while preserving stack limits, one parser thread, diagnostics and joined
shutdown. Parsing remains inside the measured worker.

Worker CPU receipts encode exact nanoseconds as full-width unsigned
MessagePack integers, marker `0xcf` and eight big-endian payload bytes.
JSON stays numeric. Compact integer widths previously made CPU counter
boundaries change counted transport allocation. Existing integer readers
accept these receipts, and decoding still accepts older compact integers.

Counted node startup initializes the five JSON Schema meta-validator engines;
counted workers initialize them before installing their resource observer.
The standard protocol factory prepares its configured batch declaration
at construction. Lazy session binding and preambles then
use the existing immutable admitted-definition cache.
Separate targeted stacks identified 3,152-byte rehashes in lazy meta-validator
and batch-schema admission. Startup pays that metadata work; workload schema
admission and validation remain complete. No steady-turn speedup is claimed
for moving this initialization.

The calibration includes intended main changes. `35ad2a50c3` expanded the
batch outcome contract and `613172eefb` changed source-location encoding.
`779b641db8` separated helper operation declarations from authored imports,
adding scoped fills and tagged operation identities to durable state.
`768e9f3b95` guards durable token encodings. `dfc0365d94` gives external
TypeScript integers binary64 Number semantics, with explicit BigInt tags,
new JSON conversion and regenerated helper plans. `8b4ea7efc0` retains
released helper definitions and exact native bindings. These contracts and
their changed code-mode transport, retained state and allocation costs are
preserved. Their individual numerical contributions were not isolated.

Main's `cc0e5b332f` reuses immutable batch schemas, checkpoint environments
and canonical request fields, and counts SQLite blob binding sizes directly.
Environment references add graph rows while reducing repeated schema bytes.
The combined ordinary population has these host allocation medians:

| Workload | Previous baseline | Current population |
| --- | ---: | ---: |
| chat-short | 8,388,543 B | 4,368,477 B |
| chat-long-compacting | 8,700,913 B | 4,681,295 B |
| ts-codemode | 11,088,014 B | 11,983,899 B |
| py-codemode | 8,275,273 B | 8,256,481 B |

Five ordinary captures at clean `4b2f81964d` matched every counted summary
and every turn/compaction record across four workloads: 1,280 turns and
15 compactions. Their executable digest is identical and no diagnostic
probe or attribution mode is enabled. The refresh uses the measured
medians, sets zero measured noise and retains the tighter prior/observed
depth slopes. The TypeScript host slope is 23,795.667 B per prior turn and remains
within the existing allowance around the retained 23,166.143 B baseline.
No depth-slope baseline rises. Hard budgets, tolerances, known-breach policy,
normal blessing guard and all determinism assertions remain unchanged.
Main's subsequent `c8680459bd` collects unreachable arena cycles before
terminal release under memory metering. A source-isolated comparison adds
160 worker bytes and four allocations to every measured code-mode turn,
with no other counted record changes. The terminal refresh attributes four
worker medians to that required cleanup, preserving all tighter slopes.
Main's `aa63937487` then shares dialect construction declarations,
expression recipes and paired statement/note buffers. Python now builds
one `Vec<Emitted>` and splits it into statements and notes; expression
recipes clone input expressions and collect call arguments. The isolated
current-main population increases the TypeScript worker median by 20,028 B
and 23 allocations, and Python worker bytes by 9,048 B. Host, IPC, SQL and
other counted medians agree. The final refresh attributes these three
worker medians to the new construction machinery. It does not assign the
individual numerical contributions to each internal mechanism.
The original full-path law separately checks the compiled baseline.

SQLite uses one fixed 256-entry command ring in production and counted replay.
Async admission waits for a free slot when the writer falls behind. Both paths
release capacity on dequeue and group the writes already queued behind the
writer, using the same batch limits and transaction implementation. The ring
reuses its storage rather than allocating blocks as scheduling changes.

Counted replay retains synchronous replies and waits for callback disposal.
The durable simulation and crash matrix need those replies so their virtual
clocks cannot advance during SQLite work. This test-only option changes reply
waiting, not the queue or write batching. Serial replay measures the production
queue's allocation cost; concurrent durable performance rows separately measure
batch sizes and throughput. Serial replay does not qualify production contention
or scheduling latency. Warmturn's retained-byte cache-tail fix remains in effect.
No PostgreSQL or quiet-host wall-time acceptance is claimed.

The compatible full-width unsigned CPU encoding removes one byte per receipt.
Against the preceding qualified population, code-mode host allocation and IPC
medians each fall by 19 B; TypeScript worker bytes fall by 4 B and Python by
5 B. The unchanged replay law passes that encoding without a baseline change.
Main's subsequent `54b4f002b6` changes Python field ordering/conversion and
resident literal, numeric-answer and certified text-join plans. With all
sixteen lane patches unchanged, its paired population reduces TypeScript
worker medians by 11,836 B and 94 allocations, and raises Python worker bytes
by 64 B. Other counted medians agree. Individual internal contributions were
not isolated. The exact combined-tree law passes determinism and rejects only
Python's median, 4,358,643 B against 4,358,584 B.

Five fresh ordinary captures at clean `d9d7c44ff7` reproduce the same changes
and match every counted summary and turn/compaction record: 1,280 turns and
15 compactions, with one executable digest and no diagnostic modes. The final
refresh records the seven changed medians, including the transport reductions.
Measured noise stays zero, and every depth-slope baseline keeps the tighter
old/observed value. Hard budgets, tolerance rules, known breaches, the blessing
guard and all strict determinism assertions remain unchanged.
