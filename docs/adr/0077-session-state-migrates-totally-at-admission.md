# Session state migrates totally at admission

## Status

Accepted. Ratified on FIG-874; this ADR defines the store-owned quadrant of
that compatibility doctrine.

## Context

Lash has exact format counters for individual mutable records: the checkpoint
manifest and component encoding, the session head, process-wake delivery, and
protocol-turn options. Those counters let a reader identify one record's
codec, but they do not say whether every record needed to continue one session
has moved together. A store can therefore pass its DDL gate and still contain
a head, checkpoint, pending input, and queued command that no single runtime
generation can safely interpret as one recovery unit.

[ADR 0045](0045-services-are-stateless-substrates-own-continuation.md) makes
the store's committed continuation sufficient for recovery. That property
requires a compatibility boundary over the complete mutable continuation, not
independent best-effort reads. The session-execution lease already supplies the
single-writer lane and monotonic fencing generation needed to move that
boundary without racing recovery.

## Decision

Each session has one monotonic `session_state_version`. It versions the
**bounded mutable continuation projection** owned by the Lash session store:

- session head and persisted session configuration;
- the current checkpoint manifest and Lash-owned checkpoint-component bodies;
- Pending Turn Input rows, including their claim and terminal state; and
- Queued Work rows for that session, including session commands, process-wake
  payloads, claim state, and interrupted-composition evidence.

The version is an independently readable scalar beside the session's durable
binding metadata, not a field hidden inside any mutable payload it guards. A
new session is stamped with the binary's current version when its binding is
created. Installing the scalar in a backend's physical schema follows that
backend's existing DDL rules; it is not a session-state migration.

The projection is bounded by ownership, not by row count. “Total” means that
every reachable in-scope value for the session is either converted by the
declared chain or causes the whole admission to fail. No row may be skipped,
reset, dropped, decoded with a guessed default outside an explicit mapping, or
left at an old in-scope representation after success. An unknown source
version or source codec refuses without mutation.

### Admission is the only migration seam

Migration is a prefix of execution admission, never store construction. Before
recovery or any new turn, the runtime:

1. acquires the session-execution lease and retains its full authority;
2. reads `session_state_version` before decoding any in-scope mutable payload;
3. refuses a version newer than the binary, or an older version for which the
   registry has no complete chain;
4. for an older supported version, runs the complete ordered converter chain
   and advances the version in one backend transaction; and
5. only after that transaction commits, hydrates recovery state or begins the
   new turn while still holding the execution lane.

The migration transaction locks the version row before enumerating the
projection and revalidates the presented lease token and fencing generation
inside the transaction. All converted rows and the final version advance are
one atomic commit. A crash before commit leaves the complete source generation;
an ambiguous result is resolved by rereading the scalar and rerunning the same
deterministic chain. A superseded or expired fence vetoes the transaction.

This gate is stricter than the existing CAS-only fallback for a busy execution
lane. A path that has not completed admission under an acquired lease may not
recover or start a turn lane-lessly. The session-head CAS remains commit
authority, but it cannot make interpreting a projection concurrently with its
migration safe.

Every ordinary reader or writer of an in-scope payload checks the scalar first.
A current-version reader then applies the record's own codec check; a current-
version writer serializes on the version row so it is ordered before or after a
migration transaction. An old-version result directs the caller through
admission migration, and a newer-version result refuses. Only the private,
lease-fenced migration path may read old in-scope payloads. This makes a
concurrent enqueue fall wholly on one side of migration: it is either included
as source state or written in the target format after the advance.

### Record counters remain codec discriminators

The checkpoint-manifest `2`, checkpoint-component encoding `2`, session-head
`4`, process-wake `1`, and protocol-turn-options `1` counters remain. They
identify the local envelope or body codec used by a converter and continue to
detect corrupt or impossible mixtures. `session_state_version` instead says
that the complete mutable recovery unit has passed one semantic compatibility
boundary. Neither number can replace the other.

At the current session version, every exact in-scope record counter must be one
the current reader accepts. Older counters are reachable only through the raw
migration decoder selected by a declared converter step. A current session
marker paired with an old exact record counter is corruption, not permission to
run a partial migration.

The FIG-1895 head change is the worked example. Under this decision it still
bumps `SESSION_HEAD_META_SCHEMA_VERSION` from 3 to 4. It also advances
`session_state_version` and registers a total step whose head mapping decodes
v3, writes v4, and materializes the newly durable `generation` field with the
declared legacy meaning (`GenerationOptions::default()`). The same step audits
queued session commands and declares unchanged old command payloads
tolerate-old where their existing representation already has identical
meaning. The session version does not absorb the head counter. The immutable
node-body generation remains a separate forward-reader concern and is owed a
bump only by an actual node-body shape change, not merely because a head-only
type shares its source file.

### Converter registry and source-shape enforcement

Core owns one ordered registry of adjacent steps. Each step declares:

- exact `from` and `to` session versions;
- stable identifiers for every in-scope source projection it maps;
- the accepted source and emitted target record codecs for those projections;
- the deterministic converter for each mapped projection; and
- any explicit `tolerate_old` declaration, with its compatibility fixture and
  reason, for a representation whose old bytes retain exactly the same meaning.

Steps are contiguous and unique. Admission composes them from the stored
version to the binary's version inside one transaction; it never searches for
an opportunistic direct converter. Conversion receives no clock, randomness,
network, host configuration, or resident state. It derives the target solely
from the complete stored source projection. A `tolerate_old` declaration is
not a waiver for an exact codec mismatch: it is valid only where the owning
reader explicitly accepts the old representation and tests its unchanged
meaning.

`scripts/check_version_bumps.py` keeps its source-projection model and extends
the inventory with an ownership and compatibility disposition for every
guarded durable shape. A changed projection in the bounded mutable continuation
must be named by the newly added adjacent session converter step, or carry an
explicit tested `tolerate_old` declaration. A change with neither fails CI.
The check also fails a session-version advance without one contiguous registry
step, a mapping whose source or target codec disagrees with its record counter,
and a guarded in-scope shape that has no disposition. Shapes outside this
domain retain their own existing version rule. This mechanical gate proves
coverage of declared source projections; the fixture and fault laws below
prove operational totality.

### Executable state remains pinned

Totality ends at the store-owned projection. A parked Lashlang segment, VM
continuation, RLM snapshot, or executable artifact is not converted by session
admission. Its instruction pointer and heap have meaning only under the exact
code and artifact identity that created them, so the executable episode stays
exact-pinned and drains under that deployment as required by
[ADR 0043](0043-hosts-register-immutable-deployments.md). The session's mutable
store data may migrate successfully while such an episode remains parked. If
the pinned code is unavailable, resuming that episode refuses; it does not roll
back the session migration or reinterpret the executable state.

An eternal subscription is store-owned data and migrates with its session. It
is not treated as one eternal execution. Each occurrence starts one finite
executable episode, and that episode pins and drains independently. New
occurrences use current code; already-started occurrences finish on their
pinned code or use the separately decided fork-forward remedy. There is no
fifth strategy that pins an entire session, lazily upgrades an instruction
pointer, or exempts an occurrence from drain.

## Enforcement gates

A source generation is supported only while all of these gates remain green:

1. **Frozen generation corpus.** One non-regenerating fixture corpus per
   supported source generation covers both SQL backends and every reachable
   mutable variant: head present and absent, config values, checkpoint roots
   and each Lash-owned component, pending and terminal inputs, both queued-work
   classes, commands and wakes, unclaimed and claimed rows, interrupted claims,
   and each reachable staged or terminal work phase. Adding a variant expands
   every still-supported generation's corpus or explicitly makes that source
   generation unsupported.
2. **Interruption, idempotence, and backend determinism law.** For each source
   generation and backend, inject interruption at every migration write
   boundary, reopen, reacquire the lane, and rerun. The result must equal an
   uninterrupted migration. SQLite and PostgreSQL must produce the same
   backend-neutral semantic projection and the same format-owned payload bytes.
   Before the atomic commit the source remains intact; after it, rerun is a
   no-op with the identical result.
3. **Refuse-newer law.** On each backend, pre-acquire a valid lease, capture the
   complete database state, stamp an otherwise valid session at
   `current + 1`, and place malformed mutable payload behind that marker. The
   admission attempt must return the typed newer-session refusal rather than a
   payload decode error, and a post-attempt comparison must prove that no row
   changed.

## Hard fences

This decision does not create a universal storage version:

- Store DDL retains its creation/open-time transactional migration or exact
  refusal policy. Session converters never alter physical schema.
- Immutable graph-node bodies retain tolerate-old/refuse-newer reads and are
  never rewritten by admission.
- Engine journals retain tolerate-old replay under FIG-1139 and FIG-1140 and
  are never rewritten by the Lash session store.
- VM, RLM, and artifact executable state remains exact-pin plus drain.
- Wire compatibility remains negotiation and refusal, never storage migration.

## Consequences

- Successful admission leaves one wholly current mutable continuation; later
  recovery has no lazy migration branches.
- Migration failure preserves the complete source generation and prevents both
  recovery and new work for that session.
- An older binary refuses a session advanced by a newer binary before decoding
  or writing its mutable state. There is no downgrade path.
- Sessions migrate independently. One session's refusal or conversion does not
  block unrelated sessions in the same physical store beyond normal backend
  transaction contention.
- Backend implementations own transaction and locking mechanics, while core
  owns the converter chain, projection membership, deterministic semantics, and
  conformance laws.
