# One growing turn through park, resume and close

`growturn` measures one SQLite-file session and one turn, submitted through
`send()`. The deterministic provider emits N TypeScript cells. Each cell
awaits one `Once` tool returning exactly P bytes of distinct text, checks its
length, and prints a success marker and the value. This is a worker-heavy
population: every round runs a cell. It measures growth within one turn,
not offered load or repeated identical large-value sharing.

Round N/2 also awaits a deferred tool. The host stops and drops its core,
boots a fresh core in the same process, checks the pinned call identity and
resolves it. Round floor(3N/4) parks again. `--phase start` exits cleanly with
that call unresolved; `--phase resume` is a separate process that opens the
same database, checks the saved identity and parameters, resolves the call,
finishes the remaining rounds and requests session deletion through public
session administration. The engine owns execution and close. The close
endpoint requires its durable tombstone. This is graceful process restart,
not a SIGKILL or crash-safety proof.

Build in a sourced Kiln fork:

```sh
kiln build //crates/lash-perf:growturn__bin \
  //crates/lash-vm-worker:lash-vm-worker__bin --config=optimized \
  --materializations final --build-report .kiln/growturn-build.json
B="$(python3 tools/buck2/outputs.py --report .kiln/growturn-build.json \
  --label //crates/lash-perf:growturn__bin --single)"
W="$(python3 tools/buck2/outputs.py --report .kiln/growturn-build.json \
  --label //crates/lash-vm-worker:lash-vm-worker__bin --single)"
mkdir -p .kiln/growturn
for phase in start resume; do
  (ulimit -v 8388608; /usr/bin/time -v "$B" --phase "$phase" \
    --rounds 1000 --payload-bytes 65536 --checkpoint-every 100 \
    --database "$PWD/.kiln/growturn/turn.sqlite" --vm-worker "$W" \
    --out "$PWD/.kiln/growturn/checkpoints.jsonl")
done
```

Start requires a fresh database; resume requires that database and its sibling
`turn.park.json`. Use independent directories for N = 10, 100, 1000 and P =
1024, 65536. On a pool box, rsync the materialized executables, use absolute
remote paths, pin with `taskset -c 2-15`, retain load/CPU-pressure and NativeLink
CPU observations, and remove the private directory afterwards. Do not alter
the shared worker service. Retain `/usr/bin/time -v` for each host process.

## Reading the JSONL

Samples occur before boot, at boot, every k completed rounds, while the
worker awaits the next sampled tool, at each park/handoff, completion, close
and 250 ms after core shutdown. `resume-*` is the fresh-core endpoint before
resolution; `resumed-*` is the first model request after the restored cell
finishes, including when that round is outside the k-round interval. `rounds_done` at `worker-await` excludes the
current unsettled payload; a park includes that round's completed payload,
while its deferred call is still unfinished. `elapsed_ns` restarts per host
PID; `unix_time_ns`, PID and phase join the two process lifetimes.

- `live_requested_bytes` is outstanding process allocator demand. Cumulative
  allocated/freed bytes and `traffic_since_previous_checkpoint` show churn;
  freed traffic is not an exact count of short-lived allocations. The heap
  profile supplies allocation-site/lifetime evidence. Harness, asynchronous
  teardown and background work are included; allocator slack and native
  allocations are excluded. The default keeps `StatsAlloc<System>` installed.
- `model_request_text_bytes` counts the text in the current model request's
  messages without copying or encoding it. `cumulative_model_request_text_bytes`
  sums those requests in this host process. These count the inherent transcript
  payload separately from allocator traffic; instructions and non-text blocks
  are excluded. Same-process reopening retains the cumulative count, while a
  new process starts it at zero.
- `/proc` rows give host and each direct VM worker's RSS/HWM in KiB, descriptors
  and threads. Children of every host thread are deduplicated and identified
  by their worker argv. A worker disappearing during observation produces an
  observation-error row. Absence of a worker row is absence at that endpoint,
  not zero worker memory. Workers are sampled during tool projection as well
  as between cells. Endpoint RSS is not continuous peak capture; HWM is per PID.
- Store quantities share one read transaction. `history_body_bytes` counts
  SQL history bodies. `blob_bytes` counts all canonical and opaque blob bytes
  currently present, including shared helper releases and any uncollected
  nodes. `canonical_by_kind_nodes_bytes` partitions canonical blobs by kind.
  Stored residency is not an exclusive live-session reachable-byte census.
- `tool_and_cell_outcome_reachable_bytes` is the deduplicated canonical closure
  of all stored `CallOutcome` roots. Those roots can be reclaimed between
  cells, so zero is meaningful. `tool_marker_text_leaf_bytes` counts canonical
  text leaves containing `grow-payload:`; it also sees rendered copies and
  excludes continuation leaves without that prefix. It is an attribution
  proxy, not exact unique payload bytes. These quantities overlap the blob
  total and must not be added to it. The read scanner copies one bounded blob
  at a time and contributes to measured allocation traffic.
- `completed`, `errors` (failed/cancelled) and `unfinished` are stored run-row
  counts. They can return to zero after close deletes rows. Preserve the
  `completed` endpoint (1/0/0), then require `close_tombstone_observed == true`, recorded only after the
  facade returns its typed `SessionDeleteCompletion::Deleted`. Engine table
  counters are outside this collector's SQL boundary.

The provider refuses to count a cell without its successful payload receipt;
both handoffs refuse changed call keys, owners, call/tool identities or
deadlines. Command failures append a failure row and exit unsuccessfully.
No memory, resource or timing threshold is enforced.

## Allocation-site attribution

The existing heap tool supports this population, preserving the host allocator
counters and collecting worker DHAT windows:

```sh
(ulimit -v 8388608; /usr/bin/time -v python3 scripts/profile_heap.py \
  --population growturn --operations 1000 --payload-bytes 65536 \
  --checkpoint-every 100 --profile-from-round 999 --out-dir "$PWD/.kiln/growturn-heap")
```

It builds optimized symbolized host/worker feature lanes through Kiln. To
reuse materialized executables, pass `--no-build --build-report PATH`.
The output directory must be fresh. `--profile-from-round` starts the host window only when the provider sees
that completed round: this example captures the last round and teardown at
N=1000. Allocations already live when it starts are excluded from DHAT, while
the process-wide allocator counters still include them. A start process that
does not reach the selected round writes no host profile. Omit this option for
a full-window capture; a small N=10 full-window run complements the large late
window by attributing startup retention. The host files are `start.dhat.json`
and/or `resume.dhat.json`; worker files under `workers/` cover bootstrap
through the first clean reset for each worker PID. They do **not** cover every
later cell. Each profile has a scope receipt. `receipt.json` requires successful
completion, close and nonempty host/worker profiles, and records executable
SHA-256 identities. DHAT-instrumented counts/RSS include profiler bookkeeping;
compare them within that allocator mode, separately from the ordinary sweep.

To attribute heap still held by a closed session's live core, use
`--profile-at-close` without `--profile-from-round`. The start process skips
host profiling; the resumed process profiles its setup, restoration and remaining
rounds, then flushes at the durable close endpoint before core shutdown. The
process still shuts down normally. This window captures restored allocations
that a late window excludes.

In DHAT, rank `tb` for allocation traffic, `gb` at global peak and `eb` at
window end. A stack outstanding at a diagnostic endpoint is not alone a
product leak. Compare N curves, same-process park/drop, restarted-process
baselines, close, and shutdown. Report shared storage, intentional history,
temporary traffic, allocator slack and unexplained residuals separately.

## Host RSS and native allocator attribution

On Linux/glibc, add `--native-memory-dir PATH` to each process invocation.
The directory receives PID/phase/round-labelled `malloc_info` XML,
`smaps_rollup` and full `smaps` snapshots. The JSONL `native_memory` object
records `mallinfo2` arena, allocated, mmap, free, fastbin and main-arena top
bytes, arena count, and thread count times arena count. SQLite counters record
all SQLite allocation bytes and the page-cache bytes obtained from SQLite's
heap allocator (a subset of the former); configured page-cache pool usage is
in slots, not bytes. If the host interposes jemalloc, its refreshed allocated,
active, resident, metadata, mapped and retained counters are also recorded.
glibc counters then describe only glibc's allocations, not jemalloc's.

These observations precede the store scanner. They are adjacent observations
in a running multithreaded process, not an atomic memory census. Raw `smaps`
separates `[heap]`, file mappings, the main stack and anonymous mappings;
`smaps_rollup` has no separate heap field. glibc free bytes measure virtual
allocator capacity, including pages already returned to the OS, and must not
be equated to RSS. Main-arena top bytes exclude free chunks in other arenas.
Rust requested bytes exclude allocation rounding, allocator bookkeeping,
native SQLite allocations, thread stacks and file-backed pages. The `/proc`
snapshots and native counters concern the host; worker RSS remains in the
existing process rows.

On recent glibc/kernel combinations, pthread stack guards can use
`MADV_GUARD_INSTALL` instead of separate inaccessible VMAs. Include anonymous
guarded stack mappings (`VmFlags: gu`, with the stack's `nh` flag here), the
main `[stack]`, and any alternate stacks; counting only `[stack]` or searching
only for an inaccessible page before a 2 MiB mapping misses these stacks.
Adjacent stacks can share a VMA, so its count is not the thread count.

`--trim-at-idle` selects a host experiment, requiring native snapshots. It
calls `malloc_trim(0)` after durable session close and again after core
shutdown, adding `closed-trimmed` and `shutdown-trimmed` endpoints beside the
untrimmed ones. Nothing trims during a turn. Zero from `malloc_trim` means
no memory was released, not a failure. A host's real idle policy must account
for its other sessions; one closed session does not establish process idle.
The library does not select an allocator or trim memory automatically.

For comparisons, use the same executable, N, payload size and pool box. Set
`MALLOC_ARENA_MAX=2` at host process startup to test a two-arena glibc limit.
To test the host with jemalloc while retaining Rust allocation counters,
interpose the installed `libjemalloc.so.2` with `LD_PRELOAD`; the host's
`StatsAlloc<System>` then counts requests served by jemalloc. This also
interposes host C allocations. The worker launcher clears its environment
before exec, so `LD_PRELOAD`, `MALLOC_CONF`, `MALLOC_ARENA_MAX` and
`GLIBC_TUNABLES` do not reach it. Pass the worker executable directly.
Record the allocator library version and
hash. Measure instructions including workers and GNU time peak RSS, alongside
close and shutdown RSS; an idle trim cannot undo an earlier peak.

Counter definitions: [glibc allocator parameters](https://sourceware.org/glibc/manual/latest/html_node/Malloc-Tunable-Parameters.html),
[SQLite status counters](https://www.sqlite.org/c3ref/c_status_malloc_count.html),
and [jemalloc statistics and decay policy](https://jemalloc.net/jemalloc.3.html).

For the growing-turn workload, prefer host-selected jemalloc C allocation
interposition with `MALLOC_CONF=background_thread:true,narenas:2,dirty_decay_ms:0,muzzy_decay_ms:0`.
This was measured with jemalloc 5.3.0 on the same optimized executable and
worker as glibc 2.43, N=1000 and P=65536, on an i9-13900 pool box:

| Host policy | RSS after close MiB | RSS after shutdown MiB | GNU time peak MiB | Instructions, billions |
|---|---:|---:|---:|---:|
| glibc default | 499.36 | 491.82 | 513.92 | 722.56 |
| glibc idle trim | 188.83 | 112.29 | 486.23 | 719.51 |
| glibc two arenas | 276.21 | 276.14 | 409.10 | 718.32 |
| glibc two arenas and idle trim | 184.38 | 107.32 | 397.57 | 721.57 |
| jemalloc with the configuration above | 169.23 | 92.94 | 338.36 | 720.10 |

Trim rows use the endpoints after the selected trim. Peaks cover both host
processes; instructions sum their performance-core user instruction counts,
including workers. Every row completed the same turn and both handoffs.
These are individual observations under varying build load, not timing or
speedup certificates; instruction differences below 1% do not establish a
ranking. Retention also varies with thread scheduling and arena placement.
The recommendation's basis is lower measured peak and idle RSS.

A host keeping glibc can select `MALLOC_ARENA_MAX=2` at startup and call
`libc::malloc_trim(0)` from its own idle maintenance policy, as the opt-in
experiment does. Trimming returns unused pages without removing live session
objects or process-lifetime caches. In this run the final Rust demand stayed
10.95 MiB and SQLite demand fell to zero at shutdown; the elevated RSS was
predominantly freed memory retained by allocator arenas. Changing SQLite's
cache default would not address that cause. The tested jemalloc policy
interposes both Rust's System allocator calls and C allocations; selecting a
Rust-only allocator is a separate composition choice to measure.

## Closed sessions in one core

Use `--phase sessions --sessions 20` with a fresh database to grow and
request durable deletion of sequential sessions in one core, without parks
or owner handoffs. Each session sends one turn with the same N/P geometry
and requires its answer and close tombstone before opening the next.
`opened-K`, `completed-K` and `closed-K` identify the endpoints for K
sessions; compare `closed-1`, `closed-5` and `closed-20` with `boot` and
`shutdown` to distinguish accumulation from process-lifetime demand.
The close flag resets at each open. Held facade/send handles are dropped
before deletion so the endpoints measure cache ownership rather than a
host-held read snapshot. Allocator totals include bounded live replay and
harness data; they are not an exclusive decoded-window byte census.

```sh
(ulimit -v 8388608; /usr/bin/time -v "$B" --phase sessions --sessions 20 \
  --rounds 100 --payload-bytes 65536 --checkpoint-every 100 \
  --database "$PWD/.kiln/closed-sessions/turn.sqlite" --vm-worker "$W" \
  --out "$PWD/.kiln/closed-sessions/checkpoints.jsonl")
```

Add `--retain-sessions` to the sequential population to close each facade
runtime while retaining its durable session. It opens a live handle at the
completed head and calls `close()`, then samples `retained-K` after the same
250 ms teardown delay used at shutdown. These samples have no deletion
tombstone and retain K answered runs in storage. This distinguishes store
cache lifetime from durable deletion. An engine actor may stay hot for its
configured idle lifetime, but releases its decoded head at the idle pass.
A live graph snapshot keeps the newest cached window warm through its
shared reader lease; the last graph reader evicts it. A reopen then reloads
its window from stored bytes. This lease is in-memory ownership only and
is absent from the graph's serialized representation.
