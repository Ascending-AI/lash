# Process recovery obeys declared disposition; abandonment is a written fact

## Status

accepted

## Decision

Every process registration declares a **Recovery Disposition** — a required
field with no default — stating what recovery may do with the row after owner
loss:

- **Rerunnable**: another owner may re-execute the work. This is the contract
  for journaled, idempotent inputs (engine rows, session-turn rows).
- **OwnerBound**: the contract binds at first start. Before any owner has begun
  execution, any worker may claim the row; once execution has started, no other
  owner may ever re-execute it — abandonment is the only recovery.
- **ExternallyOwned**: lash never executes the row at all. Closure may come from
  the explicitly unleased `complete_process` path when an external actor or a
  process-keyed workflow supplies its own single-writer authority, or from a
  reconciled Abandon Request. Recovery never executes the external work.

**Abandoned** becomes a fourth terminal state, a peer of
`Completed | Failed | Cancelled`, with a matching `ProcessAwaitOutput` arm. It
records that the owner stopped executing the work without recording an outcome:
the true result is unknowable and no cleanup is assumed to have run. The
terminal payload carries the writer (owner drain vs. recovery sweep vs.
reconciled request), the evidence that licensed it, and the timestamp. It is
immutable — an owner that reappears is fenced by its stale lease token, never
"healed" back to running.

The terminal fact has exactly one legitimate writer per path:

- **Lash-owned execution**: `complete_process_with_lease` validates owner,
  lease token, fencing token, and expiry, then appends the terminal fact and
  clears the lease in one store transaction. A renew followed by a separate
  unfenced terminal write is forbidden.

- **Graceful drain**: the owner abandons its own OwnerBound work inline at
  close, under its own live lease — the ordinary completion path.
- **Crash**: a started OwnerBound row remains non-terminal after its lease
  expires; a replacement worker never infers a terminal from process liveness.
- **Third party**: a non-owner cannot write a terminal at all. The facade lever
  writes a durable **Abandon Request** marker (who, when, why); the sweep
  reconciles it into Abandoned only once the row's lease has lapsed. The marker
  is the operator's recorded authorization to accept uncertainty; the sweep
  stays the single system writer.

Elapsed time alone never produces a terminal state. Lease expiry is exposed
read-side (lease owner identity and expiry on `ObservedProcess`) so hosts
classify staleness themselves; only owner drain or an explicit human
authorization converts uncertainty into a terminal fact.

The registry's role does not change: it records facts and holds monitors, never
links. Lash never kills, revives, or supervises anything. OS resources remain
the concern of the component that spawned them — the shell runtime kills its
own process groups on teardown, and a worker that observes its own lease lost
terminates its local children itself. On the engine tier the division is the
same: the engine owns re-invocation mechanics, and the run handler lash
provides consults the disposition before executing, completing an already
started OwnerBound row as Abandoned rather than re-running it.

Work meant to outlive every lash host is not registered as running at all: a
detached command double-forks, reports its identity as a terminal result, and is
host/OS property from birth.

Amended 2026-08-18 (FIG-1383): the detached-launch audit row is registered
before the blocking spawn, so it is not always terminal at birth. A caller that
departs inside that window leaves an outcome lash never observed, and inventing
`Cancelled` or `Failed` there would break this ADR's own rule that a terminal is
a written fact. Such a row takes the durable, non-terminal `CallerDeparted`
state instead: recovery never claims it (it is outside the live worklist),
`ProcessWorkSubstrate::await_process_terminal` refuses it with a typed error rather than parking forever, and
retention reclaims it like a terminal row. Elapsed time still terminalizes
nothing.

## Why

The recovery sweep already implements a policy: it re-runs every non-terminal
row it can claim. That policy is invisible, undeclared, and wrong for two whole
classes of work. A `shell.start` row is schema-identical to any recoverable
tool call, so recovery re-executes the command — a fresh PTY, duplicated side
effects — while the original orphaned OS child is never reaped and the row
claims `Running` indefinitely after the host dies. An `External` row is worse:
the run path's "nothing to execute" branch returns a placeholder success, so
the first sweep pass fabricates a `Success` outcome for work lash never
observed completing. Both are the same root defect: the schema cannot express
what recovery is allowed to do, so recovery guesses, and guesses identically
for rows with opposite contracts.

Making the disposition a required declaration removes the hidden policy rather
than adding management. Deriving it from the input class was rejected because
it re-hides the contract inside a heuristic; defaulting to Rerunnable was
rejected because a producer that forgets the field would silently re-ship the
exact unsoundness this decision eliminates. The pattern is well-trodden:
Temporal declares retry policy and parent-close policy at schedule time and
never re-runs work without consulting them; Nomad declares disconnect behavior
per job and gates its terminal `lost` on positive node-down evidence, never on
unreachability alone; OTP declares per-child restart and shutdown behavior in
the child spec and applies it mechanically. The one improvement over those
precedents: where Temporal's abandon writes nothing and River collapses
orphaned work into its generic discard state, lash writes an explicit,
queryable Abandoned fact with its evidence attached.

Abandoned is distinct from Failed and Cancelled because it asserts a different
thing. Failed means the work reported failure; Cancelled means someone
requested a stop and the owner cooperated; Abandoned means the observer lost
the owner — the work may have succeeded a millisecond before the host died.
Collapsing those into a reason string would force every projector to parse
payloads to distinguish facts the type system should distinguish, and the
model-visible status vocabulary would assert stops nobody requested.

Fencing is necessary but not sufficient, which is why OwnerBound exists at
all: a lease token rejects a revenant's *writes*, but it cannot un-run the
side effects a replacement execution already performed. For non-idempotent
work the only sound policy is to never start the replacement.

## Consequences

- `ProcessRegistration` gains a required disposition field; every producer in
  the tree declares one (`start_command` → OwnerBound, lashlang engine and
  subagent session-turn rows → Rerunnable, external placeholders →
  ExternallyOwned). There is no migration default: construction without a
  disposition does not compile.
- `ProcessStatus::Abandoned` ripples through every exhaustive match —
  both store schemas, the awaiter contract, the remote-protocol mirror, the
  conformance suite, and the model-visible `processes.list` status vocabulary.
  That ripple is the migration checklist working as designed.
- The recovery sweep becomes disposition-driven end to end: Rerunnable rows
  recover exactly as today; OwnerBound started rows remain non-terminal unless
  owner drain or a lapsed-lease Abandon Request authorizes Abandoned;
  ExternallyOwned rows are never claimed. The
  fabricated-success path for External inputs is deleted.
- `ObservedProcess` exposes lease holder identity, expiry, and the declared
  disposition. A pending Abandon Request is visible to observers. Stuck
  detection is a host-built read-side classification, not a lash daemon.
- Conformance gains cases pinning: sweep obeys disposition, Abandoned requires
  owner drain or a lapsed-lease reconciled request, a revenant's
  lease-fenced writes are rejected after abandonment, and owner drain
  terminalizes inline.
- Hosts that drain gracefully see their OwnerBound work reach Abandoned at
  close; hosts that crash see it reach Abandoned at the next sweep any peer
  drives. A host that wants work to survive it uses a Rerunnable input on a
  durable engine, or a detached command — never a Running row it cannot back.
