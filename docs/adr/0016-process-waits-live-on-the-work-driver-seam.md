# Process waits live on the work-driver seam

## Status

accepted

## Decision

`ProcessRegistry` is a state interface, not a coordination primitive. Registry
implementations record process rows, observer edges, leases, wake bookkeeping,
and event logs through point reads and writes; they do not expose
`await_process`, `wait_event_after`, store-local `Notify` loops, or
backend-specific polling contracts.

Process waits live above storage:

- `ProcessWorkDriver` is the process execution and coordination seam. Process
  commands route terminal waits through the driver when one is installed.
- `ProcessAwaiter` is the core fallback for local and store-only deployments:
  it performs point reads (`get_process`, `events_after`) and uses a
  `ProcessChangeHub` when the registry is wrapped in-process, with bounded
  exponential backoff when another process may be mutating the store.
- `ProcessAttach` lets an external execution backend own a terminal await. The
  Restate adapter uses synchronous ingress to
  `LashProcessWorkflow/{process_id}/await_terminal`, so the durable workflow
  promise is the long-hold mechanism instead of a database wait loop.

## Why

The old registry wait methods made every persistence adapter implement both
state and coordination. In-memory, SQLite, Postgres, tests, and downstream
registries each had to carry their own notify/poll loops, lost-wakeup defenses,
and polling cadence. That duplicated the most failure-prone part of process
waiting while hiding it behind a trait whose real job is durable state.

The runtime already has the right boundary: `ProcessWorkDriver` knows whether
process execution is inline, worker-owned, or externally attached. Storage does
not know whether a wait should be a cheap in-process watch, a bounded polling
loop, or a durable workflow promise. Moving waits to the driver seam keeps
stores simple, lets engine-backed deployments use their engine's suspension
economics, and gives store-only deployments one shared implementation.

## Consequences

- Store adapters implement no process wait loops and keep no wait notification
  fields. A watched registry decorator publishes in-process change ticks without
  changing the registry trait.
- Inline waits are still correct without a hub: the awaiter repeatedly performs
  narrow point reads with a 25ms floor, doubling backoff, and a 1s cap. With a
  hub, local mutations wake waiters promptly without database polling.
- External drivers that install `ProcessAttach` are authoritative for terminal
  waits. An attach error is surfaced instead of silently falling back to local
  polling, because the external backend owns the durable promise.
- Restate's in-workflow await path remains the durable handler primitive; the
  new ingress attach is the host-side consumer for waiting on an already
  scheduled `LashProcessWorkflow`.
- New registry implementations should not reintroduce wait methods. Implement
  state mutations, wrap with `watch_process_registry` when local wakeups are
  useful, and expose backend-specific long holds through a `ProcessWorkDriver`
  attachment.
