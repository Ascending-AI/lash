# Unleased process completion carries an explicit, validated authority

## Status

accepted

## Decision

`ProcessRegistry::complete_process` — the terminal-write path that does **not**
present a Lash process lease — takes a required `ProcessCompletionAuthority`
argument naming, and carrying evidence for, the discipline under which the caller
is allowed to terminalize an unleased row. The old bare
`complete_process(process_id, await_output)` signature, whose authority lived
only in a doc comment, is deleted outright — no deprecation shim, no compat
alias.

`ProcessCompletionAuthority` has exactly three variants, one per legitimate
unleased writer:

- **`ExternalOwner`** — an external actor closes an `ExternallyOwned` row it
  holds a session handle grant for (the `shell.start` detach path). The
  session-manager completion path verifies the caller holds a handle grant for
  the row before the authority reaches the registry; per-closer attribution is
  no longer recorded on the terminal event.
- **`WorkflowKey { workflow_key }`** — a workflow-key-coalesced substrate (e.g.
  Restate keyed by `process_id`) completes a row it ran itself. Its
  single-writer discipline is the engine's per-key coalescing, not a Lash lease;
  `workflow_key` records the key that served as that discipline.
- **`ReconciledAbandon`** — the sweep reconciled a durable Abandon Request on an
  `ExternallyOwned` row into an `Abandoned` terminal. It carries no owner: the
  closure is authorized by the recorded request, not a live writer.

Each backend (in-memory, SQLite, Postgres) calls
`ProcessCompletionAuthority::validate` against the row's declared
`RecoveryDisposition` **inside** its completion operation, before appending any
terminal event, so the contract is enforced once per backend rather than at each
scattered caller. The disposition×authority matrix is:

| authority \ disposition | Rerunnable | OwnerBound | ExternallyOwned |
| --- | --- | --- | --- |
| `ExternalOwner`      | reject | reject | **accept** |
| `WorkflowKey`        | **accept** | **accept** | reject |
| `ReconciledAbandon`  | reject | reject | **accept** (Abandoned terminal only) |

`WorkflowKey` accepts both lash-executed dispositions because Restate runs both
Rerunnable and OwnerBound rows, and its run handler terminalizes an already
started OwnerBound row as `Abandoned{Sweep}` under the same authority — it is not
restricted by terminal state. `ReconciledAbandon` additionally requires the
terminal be `Abandoned`, since that is the only outcome a reconciled request can
produce. A mismatch returns a typed `PluginError::Session` and appends nothing.

The validated authority is recorded on the durable terminal event as audit
evidence: the terminal event payload gains a `completion_authority` sibling to
`await_output` (the `/await_output` selector is untouched). This follows the
`AbandonEvidence` precedent — a terminal write says not just *what* happened but
*who was allowed to write it*. The lease-fenced path
(`complete_process_with_lease`) records no authority field: its evidence is the
lease it validates and releases in the same transaction, so its terminal payload
is byte-identical to before.

## Why

`complete_process` was a public trait method on `Arc<dyn ProcessRegistry>` whose
authority was pure convention. Any holder could terminalize any row with any
outcome; the only thing standing between a caller and an unsound write was a doc
comment and whatever ad-hoc checks each caller happened to perform. Those checks
were real but scattered and inconsistent: `complete_external_process` verified a
handle grant *and* re-read the row to reject non-`ExternallyOwned` dispositions;
the Restate run handler relied entirely on workflow-key coalescing and checked
nothing about disposition; the reconcile path checked only that the row was still
non-terminal. The single invariant they were all groping toward — *this writer is
allowed to close a row of this disposition* — was expressed nowhere in the type
system and enforced nowhere in the store.

In-process Rust cannot make such a token unforgeable, and that is explicitly not
the goal. The value is **explicitness and a single validation choke point per
backend**. Making the authority a required argument means a new unleased caller
must name which discipline it is standing on, and cannot compile without doing
so (there is no `Default`, matching the runtime's explicit-over-defaults stance
and ADR 0019's no-default disposition). Moving the disposition check into the
backend means every store enforces the same matrix uniformly, and a caller can
no longer forget it. For `ExternalOwner`, the session-manager completion path
also verifies the caller's handle grant before entering the registry; the
terminal event records the validated authority kind without copying an
unverified per-closer scope string into durable audit data.

Alternatives rejected: threading the authority only to the facade layer (leaves
the store trusting) re-hides the invariant the way the doc comment did; encoding
the authority inside `ProcessAwaitOutput` would enlarge the pervasive type that
flows through every tool result and cross the remote-protocol wire for a fact
that belongs to the write path, not the outcome; a capability object minted by
the registry would imply an unforgeability guarantee the process model cannot and
need not provide.

This does not touch the leased path. Lash-owned workers still fence every
terminal write with `complete_process_with_lease` (ADR 0019); the recovery
sweep, including its own externally-owned-abandon reconcile, claims a lease and
completes atomically through that path. `ProcessCompletionAuthority` governs only
the deliberately unleased writers whose discipline lives elsewhere. No store-side
watch/wait and no second replay journal is introduced: lash owns the
effect-journal contract, and the configured substrate owns the journal (ADR
0012).

## Consequences

- `ProcessRegistry::complete_process` gains a required
  `ProcessCompletionAuthority` argument; the bare form is deleted. Every backend,
  the `WatchedProcessRegistry` decorator, and every test double implementing the
  trait take the new signature. Construction without an authority does not
  compile.
- The three production callers now construct their authority explicitly:
  `complete_external_process` builds `ExternalOwner` from the grant it already
  verified (its caller-side disposition re-check is deleted — the registry
  enforces it); the Restate run handler builds `WorkflowKey`; both the Restate
  and core reconcile paths use `ReconciledAbandon` (the core sweep already
  reaches its reconcile through the leased path, so only the Restate ingress
  runner's unleased reconcile changes).
- Terminal event type, replay key, and payload construction are consolidated
  into shared `terminal_append_request` / `terminal_event_type_name` helpers used
  by every completion path across all backends, so the payload shape has one
  source of truth and the authority-evidence field cannot drift between stores.
- The process-registry conformance suite gains
  `completion_authority_validated_against_disposition`, pinning the full matrix
  (accepted and rejected per disposition) and the terminal-event evidence field
  for every backend; it runs under the SQLite and Postgres conformance harnesses.
- The remote protocol is **not** bumped. `complete_process` is an in-process
  registry write, not an RPC; the remote surface mirrors process *records*,
  *await outputs*, and *abandon evidence* (read-side projections), none of which
  changes shape. The new `completion_authority` lives in the opaque
  `serde_json::Value` event payload, which crosses the wire unchanged.
- The unleased `complete_process` validates the authority against the row's
  disposition and appends the terminal event as **one atomic unit** — a single
  `write_flow` transaction in SQLite, one `FOR UPDATE` transaction in Postgres,
  and the `managed` lock held across validate-and-append in the in-memory double.
  A split read-then-append leaves a window in which a paused caller could
  re-validate against one disposition, then append after the row was completed,
  pruned, and re-registered with a *different* disposition. The conformance suite
  gains `completion_authority_reads_live_disposition_not_stale`, a sequential
  proxy (complete → prune → re-register with a different disposition → retry with
  the stale authority must be rejected, appending nothing).

## Cross-version consequences

Terminal process events carry a `completion_authority` field in their payload,
which the replay-key payload hash covers. Adding that field was the first payload
format hazard: a pre-cutover terminal event had no field, so its stored hash
could not match a post-cutover retry. SQLite process databases therefore moved
to `user_version = 11`, and the single Postgres schema component (which gates
`lash_process_events` alongside every other table) moved to version 11.

Removing `ExternalOwner.granted_to` is the second half of the same hazard. A
terminal event containing
`{"authority":"external_owner","granted_to":"session:abc"}` hashes differently
from one containing `{"authority":"external_owner"}`, so a retry across this
cutover would also spuriously diverge instead of replaying idempotently. Per
lash's reject-and-recreate doctrine (no migration chain), SQLite process
databases now move to `user_version = 13` and the Postgres schema component to
version 18. Pre-cutover stores are rejected loudly at open and must be recreated
rather than comparing replay hashes across terminal payload formats.
