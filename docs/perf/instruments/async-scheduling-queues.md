# Per-operation async scheduling and physical queues

Set `LASH_PERF_ASYNC_RECEIPT` to write a functional sidecar beside an existing
population's receipt. `TaskMonitor` wraps build/seed/export, run/await turn
stages, named async checkpoint phases, each high-traffic operation kind, and
boundary send acceptance/settlement. It uses stable Tokio; neither
`tokio_unstable` nor Console is enabled. The operation wrapper is also available
as `perf_support::async_operations::observe` for further populations.

```sh
LASH_PERF_ASYNC_RECEIPT="$E/async-on.json" \
  kiln run //crates/lash-perf:lash-perf__bin -- \
  --runtime-perf-scenario durable_standard_tool_turn_sqlite \
  --runtime-perf-runs 1 --runtime-perf-warmups 0 --runtime-perf-turns 1 \
  --runtime-perf-smoke
LASH_PERF_ASYNC_RECEIPT="$E/async-off.json" LASH_PERF_ASYNC_TIMING=off \
  kiln run //crates/lash-perf:lash-perf__bin -- \
  --runtime-perf-scenario durable_standard_tool_turn_sqlite \
  --runtime-perf-runs 1 --runtime-perf-warmups 0 --runtime-perf-turns 1 \
  --runtime-perf-smoke
```

The sidecar declares its process and command window. Task records report poll
counts, busy poll nanoseconds, scheduled delay (wake to next poll), idle time
(poll to wake), and first-poll delay, summed by operation kind over that window.
Busy poll time is elapsed time inside polls, not CPU time; child operations and
nested async phases can overlap. Dropped-future counts include futures dropped
after successful completion and must not be called cancellation counts.

Physical queue records use monotonic clocks and process-local operation IDs.
SQLite workers transfer the enqueue clock with accepted work, recording dequeue
and service completion even after the caller drops its future. Write/setup/read
gate children carry their worker operation ID. Worker queue time includes its
channel wait plus measured gate waits; worker service excludes those waits.
Each physical record satisfies `elapsed_ns = queue_wait_ns + service_ns`.
Gate service is gate hold time and overlaps the worker service; never add parent
and child records. PostgreSQL work admission records bracket the actual
semaphore acquisition; their service starts after admission and includes pool
checkout and SQL, rather than claiming pure database execution or lock wait.
An incomplete admission or deadline records its partial queue/service interval.

Queue records are capped at a configured 8,192 per process/window, operation
kinds at 64; overflow counts explicitly report omitted records/operations.
With `perf-witness` disabled all store hooks compile out. With it enabled but
no active capture there are no queue clocks or allocations. Capture allocates
its bounded queue buffer once; active probes add monotonic reads, task counters,
and short recorder locks. The off recipe disables both task and queue probes,
so matched on/off command-window elapsed samples quantify combined overhead.
A single pair on this shared host is diagnostic, never an overhead baseline or
regression threshold.

Sidecars are written even if a command returns an error. Child `boundary-worker`
and `latency-worker` processes inherit the opt-in setting, open their own window,
and append `.pid-<pid>.json` to the sidecar filename so their startup and serving
queues cannot overwrite the parent's evidence. Their task receipts cover only
operations explicitly wrapped in that process. No parent task metric measures
child CPU. The former unbounded write-gate recorder and
`LASH_SQLITE_GATE_TIMING` switch are removed.
