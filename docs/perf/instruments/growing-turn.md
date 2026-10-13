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
