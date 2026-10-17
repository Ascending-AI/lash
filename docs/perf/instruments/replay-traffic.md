# Replay transport and SQLite physical writes

`lash-perf replay <workload> --store-dir <dir> --out <receipt.json> --traffic`
adds `traffic` to each round and compaction in the replay receipt. Select
SQLite file stores. The observer is installed before opening the stores;
turn cuts exclude setup and between-turn maintenance, as the existing metrics
do. Without `--traffic`, receipts omit this field and SQLite uses its ordinary
VFS. The observer is compiled only with `perf-witness`; ordinary store builds
contain no observer state or calls. There is no observing VFS callback on the
off write path. Enabled collection is diagnostic and changes measured work.

## Transport

`traffic.ipc.sent` and `received` map message kinds to integer `frames` and
`bytes`, including the frame header. They sum exactly to the round's existing
`ipc_frames` and `ipc_bytes`. Native-only workloads have `ipc: null`.

The parent classifies its existing owned/borrowed encode producers; the
receiver classifies the result of its existing decode. Classification uses
fixed arrays (10 parent bins, 16 worker bins plus `undecodable`), with no
additional decoding, payload copies, strings or logging per frame. Names are
materialized only when exporting a report. `pages` remains distinct from
`host_read`. A bundled `run` is one frame; splitting it into `start`,
`deliver` and `run` counts the actual frames. Socket read/write boundaries do
not create bins or counts. A whole frame whose decode fails still counts in
the aggregate and the explicit `undecodable` bin; replay refuses that interval.
Pool measurements also expose these cumulative bins as `counters.ipc`, in the
order of `IpcTraffic::SENT_KINDS` and `RECEIVED_KINDS`.

## Physical SQLite writes

`traffic.sqlite` observes successful VFS `xWrite` calls on connections opened
by the store while the process-local `write_observer::Collector` is installed.
Install before opening measured connections. Existing connections cannot be
retrofitted; missing writes cause the byte-conservation check to refuse.
There is one collector per process. Its 16,384-row capacity bounds the retained
attribution, independently of workload size.

Each row groups `database`, physical database `page`, `kind`, owning
`operation` and store `origin`, with integer `bytes`. Page numbers start at 1;
file-wide headers and other files have no page number. Kinds are:

| Kind | Physical bytes |
| --- | --- |
| `wal_header` | Actual writes overlapping the 32-byte WAL file header |
| `wal_frame_header` | Actual writes overlapping 24-byte frame headers |
| `wal_body` | WAL page bodies, attributed to the database page number in each frame |
| `checkpoint` | Database-file writes between SQLite's checkpoint start/done notifications |
| `database` | Other database-file writes, including initial database creation |
| `other` | Other files SQLite writes through this VFS, including rollback journals |

The observer splits overlapping write ranges at actual header/body/page
boundaries. It reads the just-written WAL header/frame metadata through the
underlying VFS to identify page sizes and addresses. Rewriting one page twice
counts both writes, including header rewrites. `write_calls` counts successful
underlying `xWrite` calls, while rows partition their bytes; one call can cross
several rows. Row bytes sum exactly to `total_bytes`. `wal_body` bytes must also
equal the existing `store_bytes_written` (`CACHE_WRITE × page_size`). Headers,
checkpoint and other file bytes are additional physical traffic. This is VFS
traffic, excluding shared-memory stores, `-shm` mmap writes, syncs, truncates,
filesystem block amplification and bytes written outside the process.

`operation` uses the current SQL operation window when present, otherwise
`sqlite.connection`. `origin` names the executing store closure. A solo write
retains its origin through COMMIT; a shared COMMIT uses
`sqlite.shared_batch_commit`, with the batch envelope's operation window.
Spill writes inside a member retain that member's origin. Setup/close writes
use `sqlite.setup_or_close` and `connection lifecycle`; background checkpoint
work uses `sqlite.background_checkpoint`. These identify the physical writer.
They do not assign a shared B-tree page to one SQL statement. A checkpoint can
copy work from earlier turns and is not a causal durability charge for the
current turn.

Cuts share the observer lock with the existing SQL counter snapshot. The
whole connection operation, including COMMIT and CACHE_WRITE accounting,
remains active until both are complete. A cut with any active operation is
incomplete. Receipts retain `active_operations`, `overflow_bytes`,
`unknown_bytes` and `failed_writes`; overflow preserves its exact residual
rather than evicting rows. Epoch changes, incomplete cuts, counter resets,
parse/read failures, failed writes, undecodable IPC or any byte/frame residual
refuse the replay report. Incomplete attribution never becomes a certified
zero. Raw pool/observer snapshots retain diagnostic counters even when an
interval cannot be exported.

The named **TRAFFIC-CONSERVATION** laws exercise socket bundling and frame
splitting, two updates to one SQLite page, a checkpoint, overflow and active
boundaries, report refusal, and a real TypeScript replay's existing totals.

## Effect phases and the first document on a worker

Run the maintained local SQLite experiment (three alternating on/off pairs):

```sh
. ./env.sh
python3 scripts/effect_phases.py capture --out .kiln/FIG-6625/effect-phases \
  --build-report .kiln/FIG-6625/effect-phases-build.json
```

It builds one optimized parent/worker group through Kiln, then sends two equal
TypeScript documents through `send()` on each fresh SQLite session. Each reads
the clock and finishes with its type. Successful replay validates every terminal
and scripted call. The `--effect-phases-off` control retains the same identity, terminal and
worker execution receipts while omitting phase timestamps. Off/on useful-work
comparisons require equal terminal result digests, model calls,
worker executions and wire frame counts. Physical byte counts remain separate: Parked/Ended envelopes carry data-dependent
checkpoint/seal identities and runtime meters. Their encoded size is not the
terminal result's identity.
Every observed population still requires exact byte conservation against its own
frame totals. The observer changes neither
billing nor dispatch. Preparation of the helper catalog and kernel library
still precedes Ready; the observer measures the spawn-to-Ready envelope without
moving that work into a turn.

Each observed process writes `turn-effect-phases.jsonl` and `critical-path.txt`.
To read a retained file again:

```sh
python3 scripts/effect_phases.py read <run>/turn-effect-phases.jsonl
python3 scripts/effect_phases.py overhead <capture>/observer-overhead.json
```

For another single code-mode workload, use `lash-perf replay <workload>
--effect-phases <path>/turn-effect-phases.jsonl --store-dir <dir> --out
<receipt.json>`. The generic reader requires two document invocations on
the same PID/start epoch and at least one complete host read. Missing evidence
refuses that comparison. This mode refuses duration-history admission; its
observer identity belongs to the phase sidecar, not ordinary replay cohorts.
The service opt-in is `WorkerService::with_effect_phases(capacity)`, selected
before its pool starts; `WorkerPool::effect_phases()` returns bounded events
and an explicit dropped count. Replay retains at most 32,768 events. Ordinary
services retain no phase recorder and take no additional phase timestamps.
Events contain identities, endpoints and encoded frame sizes, never payloads,
arguments, results or copies of committed state.

All timestamps are host `Instant` nanoseconds since observer creation. Worker
progress is observed when decoded by the host, including socket/scheduling
and decode delay: it is neither a worker timestamp nor CPU time. A host-read
ID is scoped by pool, lease, owner/frame epoch and worker PID/start epoch;
the turn comes from explicit event cuts, and document/owner from the lease.
The header preserves the replay producer's identity and experiment, including
configured backend policy, worker geometry and observer settings, plus host and
worker artifact SHA-256 and terminal evidence. The replay experiment's round
window stays distinct from the phase recorder's startup/lease window. PID reuse never joins effects.
No deployed exporter, tracefs or eBPF is needed or claimed.

The text prints one host-read critical path from decoded request to observed
Computing acknowledgement. Its host-body interval includes request value
decoding, the host callback and answer value encoding. Answer frame encoding
and socket send remain separate. Dispatch, codec and resume residual includes
the other host work and worker/socket/scheduling delay. The request frame's
receive wait precedes this critical path and can include earlier guest work.
Every frame's bytes include the header; all recorded frame events reconcile
against cumulative pool frame totals, including Ready and resets. Effect
frames are a subset of those totals, never an added traffic charge.

First/second document rows compare each turn/owner/document's first activation,
from document start to first park/end, on the same worker. They retain checkout,
preceding prepare/dispatch time, frame totals and encode/send/receive/host-body
phase totals separately. Re-executing the same document in a later turn is a
second invocation even though the owner still names the same session frame. They do not claim whole-turn
latency, isolated compiler time or preparation subphases within pre-Ready
startup. Startup, checkout, document and receive intervals can nest or overlap;
never add these envelopes. Missing endpoints, worker loss, dropped events,
undecodable frames or a frame residual refuse a qualified report.

`observer-overhead.json` retains experiment identity, raw paired
`perf stat -e instructions:u` counts, perf running fields, load1 and command
peak RSS. Instructions include the host and inherited workers, startup and
export. The observer cost includes bounded event allocation and sidecar
serialization. Each population runs under an 8 GiB address-space cap and
`/usr/bin/time -v`; retained stores are fresh per sample. Temporary files and host staging live
under that sample's own directory. The text reports raw
fractional instruction deltas and their median, with no threshold. Wall time
on this shared host remains unqualified. Build once and reuse its exact
report with `--no-build --build-report <report>` for subsequent captures.
