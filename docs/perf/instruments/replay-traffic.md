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
