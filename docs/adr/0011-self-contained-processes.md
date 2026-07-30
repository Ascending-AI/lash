# Self-contained processes: capture-at-creation, no session binding, host-policy lifecycle

A Runtime Process is a standalone durable entity — id, input, captured execution
environment, event log, status, leases. It never holds a live reference to the session
that created it: the execution environment (plugin options, policy, lashlang
module/host requirements refs) is captured at creation as immutable content-addressed references,
and the durable worker always executes against an ephemeral runtime instantiated from
that capture — it never rebuilds the originating session. Session relationships are
explicit, orthogonal, optional edges (originator and caused_by as pure provenance, a
0..1 wake target, 0..n weak observer edges), none of which implies another and none of
which implies cleanup: deleting a session erases only the session's side of every edge
(its observer edges, pending wake deliveries addressed to it, its trigger subscriptions) and
never cancels a process; lifecycle remains host policy.

## Considered Options

- **Live session binding (status quo)**: the worker rebuilt the owner session's runtime
  per execution and session deletion auto-cancelled zero-grant processes. Rejected:
  processes could not outlive or exist without sessions, "what the process sees" was
  unreproducible across recovery anyway, and ownership silently bundled execution,
  wake routing, and lifecycle into one field.
- **An owner enum (Host | Session) with bundled semantics**: rejected as the same
  coupling wearing a costume — cleanup, wake routing, and execution still hung off one
  concept.
- **A configurable "host surface" for session-less execution**: rejected — capture-at-
  creation makes every process carry its own environment, so no ambient surface concept
  is needed.

## Consequences

- Processes outlive sessions structurally, and products can use processes with no
  sessions at all; sessions are one kind of creator/client.
- Capture is spec-only and immutable: fresh plugin instances per execution, no creator
  session state; process arguments are the only state handover. A mutable name must
  never be captured into an environment — that would be a live coupling in disguise.
- A process-created session is an ordinary session recording `caused_by` (downstream
  provenance); its usage is its own — there is no live usage channel to the originator.
- Wake fan-out is deliberately not a process feature (wake target is 0..1); the
  trigger bus is the pub/sub path.
- Persisted process and trigger-subscription records from before this change do not
  deserialize (pre-1.0 break, no shims).
