# Trace flight recorder

A host wraps its trace sink with `lash::tracing::FlightRecorderSink` and supplies
settings plus a callback taking an owned `FlightRecorderSnapshot`. The callback
can log or enqueue the snapshot using the host's own exporter. The wrapper does
not capture stacks or start any background work.

```rust
use std::sync::Arc;
use lash::tracing::{FlightRecorderSettings, FlightRecorderSink, StderrTraceSink};

let sink = FlightRecorderSink::new(
    StderrTraceSink::default(),
    FlightRecorderSettings::default(),
    |snapshot| {
        // Hand this owned metadata snapshot to the host's bounded export queue.
        eprintln!("flight recorder: {snapshot:?}");
    },
);
// Install on the host's LashCoreBuilder:
// builder.trace_sink(Arc::new(sink));
```

Settings default to **256 records**, a **five-second** slow threshold, snapshots
on **failed operations**, and a **60-second** minimum interval. Capacity is a
`NonZeroUsize`; hosts may tune every setting. A duration must strictly exceed the
slow threshold. A failure and a slow duration on the same record produce one
snapshot. The first eligible trigger snapshots immediately.

Snapshots hold recent metadata in append order, including the triggering record,
up to the configured capacity. `evicted_records` counts records displaced since
the previous snapshot, or since construction for the first snapshot. It saturates
at `u64::MAX` and resets only on a snapshot. Suppressed triggers still enter the
ring and contribute to subsequent context and eviction counts.

The recorder reads no wall clock. Minimum spacing uses incoming record
timestamps; a timestamp older than the last accepted trigger is suppressed.
Spacing is measured from accepted triggers, rather than suppressed ones. For
concurrent appends, ring order follows acquisition of its mutex. Callbacks can
run concurrently and finish out of order.

Metadata contains event kind, record timestamp and id, session/turn/graph/effect/
model-call identities from the context, and tool/process/engine/attempt identities
when the event supplies them. It retains a content-free outcome classification
and available duration evidence. Failures use `TraceEvent::is_failed()` plus a
failed provider-attempt outcome. Completed, cancelled, aborted, interrupted and
agent-frame-switch outcomes remain distinct. Cancellation alone does not trigger.

Reported durations come from tool, code, model-call and prompt-composition
completions. Domain completions use their retained start and the record timestamp;
provider-attempt completions use the observation's start and end. Missing or
reversed endpoints yield no duration. Events without timing evidence, including
turn terminals, do not gain inferred durations: the recorder maintains no separate
start map. Provider/stream elapsed times are stream progress, not operation
completions, and do not trigger slow-operation snapshots.

The ring never copies event payloads, arbitrary context metadata, diagnostic
messages, tool arguments/results, prompts, responses, or provider bodies. This
holds even under `TelemetryContent::Captured`. Identity strings must be identities
rather than host-encoded content. Incoming records are forwarded unchanged to the
inner sink, whose existing content policy still applies.

Each append allocates/clones only the allowlisted identities, projects scalar
metadata, and takes one short mutex lock. Retention costs O(capacity × metadata
size); capacity bounds record count, not the bytes of variable-length identity
strings. A trigger clones at most capacity metadata entries into an owned
snapshot, temporarily adding that allocation to the ring. The callback runs
synchronously outside the lock after the inner append, even if that append fails;
its cost therefore adds to append latency. Use a bounded queue for expensive
exports and choose a policy for queue saturation. The wrapper returns the inner
append/flush result unchanged and introduces no files, threads, timers or IO.
