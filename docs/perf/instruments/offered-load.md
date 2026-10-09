# Offered load

```sh
E="$PWD/.kiln/<ticket>"
kiln run //crates/lash-perf:lash-perf__bin -- offered-load \
  --population high-traffic --rates 5,20,80,320 --operations 48 --sessions 4 \
  --store-dir "$E/offered-store" --out "$E/offered-load.json"
```

Run it through `kiln run`: the high-traffic turns execute in the VM worker,
which a bare materialized executable does not find. The store directory must
be fresh. The command writes the receipt and, beside it, every operation's row
in `<out>.ledger.json` (`--ledger-out` moves it).

`--rates` is an ascending sweep in operations per second. Each step opens a
fresh SQLite store and population and schedules `--operations` arrivals:
operation `n` is due `n / rate` after the step opens, whether or not earlier
operations have finished. Arrivals go to `--sessions` sessions by ordinal; a
session runs one turn at a time, so an arrival on a busy session queues with
its clock running. At most `--max-in-flight` operations are sent and not yet
completed; an arrival beyond that waits for a slot, also with its clock
running. A step closes `--drain-timeout-ms` after its last scheduled arrival
and counts what is still open as unfinished.

| Population | What one operation is |
|---|---|
| `high-traffic` | A `send()` on the runtime-perf high-traffic population: one core submits and serves, with the `--mix` of plain, tool, queued and child turns. |
| `cross-worker` | A `send()` from a core that serves no sessions, executed by a child durable node on the same SQLite file and followed to its persisted answer. |

Each ledger row has the operation's key and five timestamps, in microseconds
on the generator's monotonic clock since the step opened:

| Timestamp | Instant |
|---|---|
| `scheduled_us` | The arrival's due instant. Configured, not measured. |
| `sent_us` | The generator calls `send()`. |
| `admitted_us` | A store poller first reads the input as taken by a run. |
| `settled_us` | The store poller first reads the run's terminal. |
| `completed_us` | The send's outcome returns to the caller. |

`admitted_us` and `settled_us` are observed by a poller that backs off from
2 ms to 50 ms, so each can be up to one poll interval late. On `high-traffic`
the poller reads through the serving core's own store. A row's `outcome` is
`completed`, `error` or `unfinished`.

Each step of the receipt reports:

- `offered_rate` (configured) and `achieved_rate`, the departure rate between
  the first and the last completion, with their ratio;
- `counts` of scheduled, sent, admitted, settled, completed, failed and
  unfinished operations;
- `intervals`, one distribution per pair of timestamps. Percentiles are
  nearest-rank over the exact samples up to `p99_9` and `max`, the
  power-of-two buckets are unclipped, and `censored` counts the scheduled
  operations with no sample. `scheduled_to_completed` is the offered-load
  latency; `sent_to_completed` leaves out the wait before the send;
- `slowest`, the slowest completed operations with every timestamp, and the
  keys of the unfinished and failed ones.

Every number carries its quantity, unit, window and statistic. A `configured`
statistic is an input or an upper bound, not a measurement.

`knee` names the lowest swept rate that fell behind and the highest rate below
it that kept up. A step has fallen behind when it leaves operations
unfinished, when its achieved rate is below `--knee-min-achieved` of its
offered rate (default 0.95), or when its `scheduled_to_completed` p99 exceeds
`--knee-p99-ratio` times the lowest rate's (default 3). The knee lies between
those two rates; a finer sweep narrows it. When no step falls behind, the knee
is above the highest swept rate. A step with few operations can cross the
achieved-rate criterion on latency jitter alone.
