# Two first-class RLM dialects with full parity and session pinning

## Status

Accepted.

## Context

RLM execution has had one authored language. Adding TypeScript could be framed
three ways, and the three have very different costs.

It could be a **migration**: TypeScript replaces Lashlang, which acquires a
deprecation clock and a compatibility story. That discards work that is not
replaceable on any near horizon — the workflow-graph lens (ADR 0037) and its
canonical printer, gradual value types through to the editor (ADR 0038), the
judged runbooks and examples built on them — and it forces every host on
Lashlang into a move it did not ask for.

It could be a **second-class addition**: TypeScript ships as an experiment,
without the runbook battery and example coverage the first language carries.
That is cheap on the day and dishonest afterwards. A dialect that hosts run in
production while its evidence is thinner than the other's is not an experiment;
it is an unmeasured surface.

Or both can be permanent and equal, which costs a doubled battery forever. That
cost is real and has to be decided in the open rather than discovered by a lane
that quietly skips it.

## Decision

Both dialects are first-class, permanently, at full parity.

Neither dialect is deprecated by the existence of the other, and no deprecation
clock is implied by anything in this programme. Each carries the full judged
runbook battery and its own example coverage, each independently, and both gate
a release. The doubled cost is accepted explicitly: a dialect that cannot pay
for its own battery does not ship. Where a host-facing runtime feature has
meaning in both dialects, "what does this mean in the other dialect" is a
review question, not a follow-up.

Parity is about evidence and support, not about identical surfaces. Two places
where the surfaces deliberately differ:

**The workflow-graph lens stays Lashlang-only.** ADR 0037's lens laws demand a
canonical printer whose source → graph → source round trip is an exact textual
fixpoint. Lashlang has one. Committing to the same laws over an ECMA-262 surface
is a substantially larger piece of work with its own design questions, and the
lens's consumers today are Lashlang workflows. A TypeScript canonical printer
and lens are separate future work with their own ADR, not an obligation of the
TypeScript dialect's first release. Until then, features that reach hosts
through the lens are Lashlang-only, and that is a stated limit rather than a
gap in parity.

**Naming is `typescript`, spelled out, everywhere.** The language id, the
snapshot engine id, host configuration, telemetry, and documentation all use the
same token. No `ts`, no `js`, no aliases, and no per-surface spelling — a
registry keyed on the language id cannot afford two names for one dialect, and a
host reading a persisted engine id cannot afford to guess. The existing dialect
registry resolves on exactly this id.

**The default stays `lashlang`.** A host that configures nothing gets what it
has today. Choosing TypeScript is an explicit act by the host, per session.

### The dialect is pinned at session creation

A session resolves its dialect once, when it is created, and records it in its
execution state. It never switches. A cell that names a different registered
language is a typed refusal — the registry distinguishes "not registered" from
"registered but the session is pinned to another" — and neither refusal falls
back to the pinned dialect or silently reinterprets the cell.

This follows the shape of ADR 0030: the properties an execution's whole history
depends on are resolved once at open rather than re-decided per turn. Here the
reason is sharper than consistency. The execution state a session carries —
globals, the heap, a parked continuation — is dialect-lowered (ADR 0060).
Lashlang's state has had aliasing erased from it by compiler-inserted isolation
copies; reinterpreting it under a reference-semantic dialect would present the
guest with an object graph whose sharing was destroyed before it ever ran, and
the reverse direction would hand Lashlang a graph its durable-boundary validator
refuses. There is no coherent mid-session translation, and inventing a lossy one
would be worse than refusing.

The mechanism is a typed choice at the create contract rather than a string on a
turn: hosts select the dialect when they build the session, an omitted selection
is the Lashlang default, and an unknown language is a create-time error instead
of a late execution failure. The choice becomes durable at the session's **first
commit**, so a session opened and dropped before any turn commits has recorded
nothing and the next open may choose again. From that commit on the recorded pin
is authoritative: reopening without asking keeps it, reopening while asking for a
different dialect is refused with a typed error naming the recorded one, and a
per-turn option cannot re-point a session that has already recorded its choice.
Subagent sessions inherit their parent's — a session tree is one dialect in v1,
because a child that silently reverted to the default would make a parity row
whose evidence contradicts its own label. `docs/rlm.html` documents the host-
facing surface and is not restated here.

A host that wants the same conversation in the other dialect creates a new
session. ADR 0047 already gives it the move: history is shared and branches are
sessions, so the new session can carry the history without carrying the
execution state.

### Cutover is wholehog, with one documented breaking window

There are no migration decoders anywhere in this programme, at any format
boundary. Durable format versions move as the substrate layers land — bytecode,
continuation, and VM ABI — and an older artifact fails closed on the exact
version check rather than being coerced by a compatibility path. Parked durable
segments drain or are recreated across the cutover. Foreground snapshots taken
under an older format fail closed rather than being read on a best-effort basis.

The whole programme lands as one stacked chain merged atomically, which is what
makes a single window possible: one cutover to announce, one drain to schedule,
one version of the operational instruction. Landing the layers independently
would spend a breaking window per layer for no gain, since no intermediate layer
is independently useful to a host.

## Consequences

- Every release is gated by two full batteries. Runbook and example work is
  planned as a doubled cost from the start, and a change that lands green in one
  dialect is not done.
- Hosts on Lashlang need do nothing, ever, as a consequence of TypeScript
  existing. There is no deprecation clock to track.
- Host-facing features that depend on the workflow-graph lens are Lashlang-only
  until a TypeScript lens exists, and that limit is documented rather than
  worked around.
- Sessions are dialect-homogeneous by construction, so no code path anywhere has
  to handle mixed-dialect execution state, and a persisted engine id is a
  reliable statement about how its state must be read.
- Deployments take one breaking window: drain or recreate parked processes, and
  expect old snapshots to be refused rather than migrated.
