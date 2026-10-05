# 0131: Durable types declare their version surface

## Status

Accepted. Stored formats and the journal logic epoch remain frozen until 1.0.

## Decision

A durable root declares `DurableRecord::SURFACE`. A journal operation declares
`JournalStep::{Output, SURFACE, KIND}` and supplies an occurrence identity.
The controller accepts that step rather than an independent string and serde
type. Each kind has one output type. Process completion and recovery from lost
substrate have different kinds; process command names carry no `:v1` suffix.

The format gate collects these implementations and follows their types through
Rust imports. Hand root lists disappear where ownership declarations reach
those roots. Explicit guards remain for DDL, encoders, hash domains, generic
wire families, and closures shared with another surface. Step kinds join their
surface signature. The journal tripwire compares command order and control
flow rather than type declarations or payload fields.

Typed raw carriers defer body decoding until the existing generation check.
They name the concrete payload type for closure discovery without moving its
Serde decode ahead of the folded sentinel.

SQLite's sealed finalize authorization declares its bootstrap recovery surface.
Its format is checked before decoding the body. A typed drained generation
retains the build and drain marker, and one validated map records plugin writer
moves. A sealed authorization is the durable decision: recovery checks its
store, stamps and epoch transition rather than retaining and re-evaluating an
operator drain report. Suspended cell state declares the enclosing RLM snapshot
surface; typed bindings need no independent inner version.

## Separate journal versions remain necessary

Keep the existing drain counters and payload stamps; do not fold them into
`JOURNAL_LOGIC_EPOCH`. Their redundancy has not been established:

- `process::decode_stamped_request` admits requests at stable process entry
  points by their payload stamp before decoding. The epoch is a build
  generation input, not this decoder's admission contract.
- `controller::live_frontier::pass_frontier` reads its marker through the
  effect-journal stamp without a folded build sentinel. `recorded_frontier_mark`
  refuses another version before decoding the mark.
- Host handler journal steps use their wire surface; their invocation does not
  acquire a Lash generation sentinel merely by using the controller context.
- Turn outcome object values use fleet-selected migration formats. They remain
  readable independently of which generation runs a journal.

The counters continue to feed generation composition under ADRs 0106 and 0115.
The epoch protects execution decisions; declared surfaces protect stored
representations. Folding requires a proof that every reader reaches generation
admission before decoding, including stable and host entry points.
