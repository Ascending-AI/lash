# Send-to-completion latency

```sh
LASH_LATENCY_ARTIFACT_DIR="$E/cross-worker" \
  just latency-gate --cases cross-worker --lanes 1 --scale-down
LASH_LATENCY_ARTIFACT_DIR="$E/poll" \
  just latency-gate --cases poll --lanes 1 --scale-down
LASH_LATENCY_ARTIFACT_DIR="$E/grace" \
  just latency-gate --cases grace --lanes 1 --scale-down
```

Each remote case launches a child durable node over one shared SQLite file.
The submitting core and marker observer serve no sessions. Child startup has
an explicit readiness handshake; closing its stdin requests node shutdown,
with a bounded kill-and-reap fallback. A child is killed if the case exits early.
The host submits only through `send()` and follows the persisted answer.

`cross-worker` uses production observer pacing. `poll` delays the terminal
wait's poll so the normal follow poll observes settlement. The historical
`grace` case extends the follow-poll ceiling to 120 seconds while preserving
the 25 ms floor used by binding discovery, alongside the remote terminal wait.
The current follower has no shift attach or live-report grace.
Both delayed paths still perform their first store read immediately. The
report's `follower` field names the current mode, and `poll_detect_ms` remains
a simulated 25 ms to 1 s schedule, not another measured clock.

Every latency case is a `service-diagnostic`: a lane sends its next sample
when the last one returns, and each span starts at the send. For
cross-worker send-to-completion under scheduled arrivals, run
`lash-perf offered-load --population cross-worker`.

These diagnostic runs retain completed samples but exit 2 because the fast
case did not run. This is functional proof, not a passing latency gate. The default
`just latency-gate` still runs every case and requires 10,000 fast samples,
overhead p50 below 50 ms and p99 below 250 ms.
The load wrapper records load and CPU pressure through the run and summarizes
each case's window (`cases.<case>.load`), but load never changes the exit
status (Sam, 2026-10-19): the wrapper exits with the child's status, and the
latency report's own verdict (budget and population rules) decides the gate.
Judge from the recorded load, PSI and work counts whether the host explains a
result, and rerun on a quiet host only when it does.

The receipt retains every sample in its referenced ledger. `clocks` contains
absolute Unix nanoseconds (join to `latency-load.json`'s `unix_s`) and Linux
`CLOCK_MONOTONIC` nanoseconds (compute phases across parent and child on the
same host). It records request, durable acceptance, acceptance acknowledgement,
engine admission, engine terminal, host completion, first activity, first
visible event and first actual assistant/reasoning delta separately. A missing
delta stays null; another activity never substitutes for it. The serving
engine's outward publication supplies these events for every topology; the
host's own first-event clocks are retained separately where it receives them.

Acceptance, admission and terminal clocks bracket successful physical commits.
Their phase boundary is the instant immediately before COMMIT; a rollback or
failed COMMIT publishes no marker. `terminal_commit_returned` is the upper
bound. This bracket is deliberate: another process can read the terminal
before COMMIT returns, so the return alone cannot be a causal terminal clock.
The terminal-to-completion phase includes the remaining commit time, whose
uncertainty is visible in the bracket. Acceptance acknowledgement is also
separate because the engine can admit before the sending task resumes.
Admission and terminal discovery clocks are taken after keyed asynchronous
reads, and their lag is reported separately. No interval is clamped to zero.

Cross-worker children export a typed, transient witness on clean shutdown;
the parent joins it by input/run identity with its acceptance and completion
clocks. Provider durations come from the executing provider in both topologies.
Each case records `baseline.kind`, `baseline.sha` and dirty-tree status:
`same-run-provider-duration` for deterministic fixtures and
`same-run-engine-model-span` for HTTP. These are same-run subtractions, not
historical inline A/B measurements. The environment also retains the executed
binary's SHA-256. `lanes` is the configured limit; `actual_lanes` is the number
of loops launched after ceil-partitioning, including loops that later fail.

`sqlite_work` reports physical write transactions/send, writer busy ms/send,
reader busy ms/send and every worker's PID, thread identity, role, total busy
nanoseconds, transaction count and busy ms/send. Busy counters compile only
with `perf-witness` and activate only while the latency collector is installed.
They cover connection callback and catalog work, including SQLite busy waits
and commit I/O, but exclude the worker queue and in-process read/write gate
waits. A write transaction is a successful BEGIN IMMEDIATE, including one
that later rolls back; it is not a commit count. Reader totals sum concurrent
threads and must not be interpreted as a latency phase.

Counts are deltas over the case's lane work, from after topology startup to
before teardown, divided by completed measured sends. They include session
creation, observer reads, background duties and, for `busy`, held predecessor
turns (declared by `includes_held_predecessors`). Remote receipts include both
parent and child workers so the serving writer can be inspected separately.
Each sample also retains cumulative parent-process worker counters at request
and completion. Concurrent sample windows overlap; subtracting those counters
cannot attribute a concurrent operation to one send. Use the case totals for
per-send cost, and per-thread rows for the writer-floor analysis.

The load wrapper samples both case edges as well as the periodic interval.
Rows label `source=case-edge` with their case and edge. Case windows retain
`interval_samples` separately: two endpoints of a subsecond case are useful
observations, not proof of continuous load coverage. The human summary prints
absolute case clocks, the ledger path, baseline identity, actual lanes and
physical work alongside phase distributions. A short functional receipt is:

```sh
(ulimit -v 8388608; /usr/bin/time -v \
  kiln gate lash "$fork" -- just latency-gate \
    --fast-samples 4 --lanes 8 --scale-down)
```

This intentionally exits 2 for an undersized fast population. It demonstrates
populated instruments without certifying latency or changing the full gate.

## Chat-loop spans

Every sample also carries `loop_clocks` and `loop_spans`, and every case
reports `loop_ms` percentiles for the spans it exercises (FIG-6841). They
come from absolute `CLOCK_MONOTONIC` stamps the harness's own fixtures take,
not from engine changes: the deterministic provider stamps each call's start
(the engine's request reaching it), each delta it emits and its return; the
benchmark echo tool stamps its own execution; the host's activity sink stamps
the deltas and recorded checkpoints it receives. A cross-worker child exports
its stamps beside its witness. A sample owns the stamps that start inside its
own request→completion window on its session.

| Span | From → to |
| --- | --- |
| `request_to_provider` | send → the turn's first provider request |
| `previous_completion_to_provider` | the lane's previous host completion → this turn's first provider request (back-to-back next turn, harness re-send included) |
| `session_open` | session create and open → the first send (`cold`) |
| `first_delta_overhead` | send → first host delta, less the provider's time to emit it |
| `delta_delivery` | provider emitted a delta → the host received it, per delta |
| `model_to_tool_start` | the model returned a tool call or cell → the tool started |
| `tool` | the tool's own execution |
| `tool_to_recorded` | the tool returned → the host saw the next recorded checkpoint |
| `tool_to_next_request` | the tool returned → the next provider request |
| `tool_loop_overhead` | the model's tool call → the next provider request, less `tool` |
| `steer_request_to_accept` | a steered send → its acceptance acknowledgement |
| `boundary_to_steer_seen` | the running call returned → the request carrying the steered input |
| `steer_send_to_model` | the steered send → the request carrying it |

The chat-loop cases are reported, never gated:

- `cold` opens a fresh session for every sample, so each send is a first turn.
- `delivery` streams 32 deltas 5 ms apart, so each delta's delivery is
  measured on its own rather than in a burst.
- `tool` makes one `benchmark_echo` call in every turn, then answers.
- `cell` serves the RLM protocol: a TypeScript cell calls `benchmark_echo`,
  then a second cell finishes. It needs the kernel worker executable, so it
  runs only when named, with `LASH_VM_WORKER` pointing at a built
  `//crates/lash-vm-worker:lash-vm-worker__bin`.
- `steer` holds the turn's provider call, sends a second input with
  `ingress(active_turn(..))`, and releases the call once that input is
  accepted. The held call answers with a tool call: a steered input joins the
  running turn at its after-work checkpoint, while a plain final answer would
  finish the turn first and defer the input to the next turn.
- `cross-worker-tool` is the `tool` loop served by a child node.

```sh
LASH_VM_WORKER="$worker" LASH_LATENCY_ARTIFACT_DIR="$E/chat-loop" \
  just latency-gate --cases fast,cold,stream,delivery,tool,cell,steer,cross-worker,cross-worker-tool \
    --lanes 1 --fast-samples 1000
```

This exits 2 for the undersized fast population; it is a diagnostic, not a
gate run.
