# ADR 0129: The transcript row stream is the only chat projection

Status: Accepted

## Decision

Hosts render committed conversation history from `lash::transcript`. They do
not choose an answer from `TurnOutput`, inspect protocol payloads, classify
committed parts, or correlate messages by parsing an ID.

The stream is a total fold of retained committed nodes. Every node contributes
exactly one row or a row carrying a named suppression. Core owns node-backed
identity, the recorded timestamp, source order, and typed turn/input provenance.
A protocol's pure `TranscriptRowProjectorPlugin` supplies neutral display
content or a suppression; the RLM implementation owns its payload decoder.
Core renders the marked reply part and resolves part rendering through the
owning representation. Live observations remain provisional.

FIG-972, FIG-5288: the UI owns its input rows. A UI correlates the opening
input by `provenance.turn_id` and every input by `provenance.input_id`, retaining
its own identity and attachments. The host computes the input ID before
sending, using `LashSession::input_id(&send_id)` or
`DurableSession::input_id(&send_id)`, and supplies that same send ID through
`SendBuilder::id`. Steering inputs can share a run's turn ID; their input IDs
identify them individually. A coalesced opening or terminal-withheld follow-on
commits one user message per admitted input, in admission order, with each
message naming exactly its own input. Its application evidence names that
message. Hosts do not derive correlation from either ID's spelling. The
workbench persists its input row's ID and creation time with the active-turn
claim, so a web process can recover that provisional row while the send is
unfinished.

FIG-984: every settled turn has one committed reply. Its writer depends on
termination: standard output, an RLM finish, a terminal tool, or a stopped
turn's committed text. The writer supplies the existing sealed `TurnReply`;
the read projection consumes it. Host code never mints a reply marker.

`DurableSession::transcript()` walks retained ancestry across frame boundaries,
without restoring a writer or admitting a turn. `SessionReadView::transcript()`
projects its captured committed graph. Raw read views remain available for
evidence, tools and auditing; their availability does not authorize a second
chat projection.

## Ordering, transport and sinks

`RowOrdinal` is snapshot-local and opaque. It supports ordering but has no
serialization, integer conversion, display implementation, or cursor meaning.
`RowId` supports equality and transport without a public string accessor.

`TranscriptRowRecord` carries the display data without an ordinal. Remote
`Committed { rows }` carries only the rows produced by that commit. It is a
freshness event, not a complete-history response or a durable change-feed
cursor. A missed event requires an authoritative read: the committed-turn read
below.

## Committed-turn read (FIG-5297)

A host that post-processes a conversation (memory extraction, analytics,
indexing, sync to another store) reads the session's committed turns after a
cursor it keeps: `DurableSession::committed_turns(after, limit)` returns
`CommittedTurnsPage { turns, next }`, oldest first by commit. Each
`CommittedTurn` carries the turn id, its committed rows, its commit time and
its typed `TurnCommitOutcome`. Its rows are this ADR's fold over exactly the
nodes the turn's commit appended, every admitted input's user row among them
(steering and coalesced inputs included); a protocol event belongs to its turn
even before the turn's first message. A host persists `next` only after
applying the page.

The order is the session's head revision, not the deployment's turn feed. A
session's commits are serialized (the session-keyed lock and the head row
lock), and each publishes the next revision, so a turn that commits late takes
a revision above every one a reader has seen: the cursor never passes it, and
a strict `>` never repeats one. Each turn's receipt records the revision it
published (`runtime_turn_commits.head_revision`, unique per session), and the
read pages its outcome-bearing receipts by it, then reads their appended nodes,
in one snapshot. The deployment feed (`turns_changed_since`, FIG-5276) was not
chosen: on PostgreSQL every read would first sequence the whole fleet's staged
changes, its cursor is fleet-wide and would need a per-session index besides,
and its receipts of deleted sessions are reclaimed under it.

`CommittedTurnCursor` is opaque and serializable. It names its session and a
revision, so it holds across process restarts, compaction (a session command
that commits no turn) and frame switches (a revision is no frame's). A fork's
numbering starts at its creation and its inherited history is its ancestor's
commits, so a fork's first read serves only the fork's own turns. A cursor of
another session is refused as `StoreError::CursorForeignSession`. A node its
session's retention reclaimed (on an abandoned branch) is absent from its
turn's rows; the turn is still served. There is no wait
variant: hosts poll.

A host can retain canonical records in its own storage. Agent-service's SQL
mirror and Slack's pending/post records are such sinks. Reasoning, code, tool, and answer writes consume canonical records.

## Enforcement

Five compile-fail fixtures seal committed classification, reply construction,
row identity, snapshot ordinals, and the protocol decoder. Exhaustive in-crate
matches name every row kind and suppression.

`scripts/check_transcript_projection.py` independently scrapes committed-truth
reads and every turn-output accessor call. The reviewed registry pins their
counts and dispositions, rejects host classification and ID parsing, and ties
every rendered asset to the shared production-JavaScript harness.

The store-to-row law checks every really committed node without a predicate.
The shared harness checks quiescent rows against production renderer blocks,
including suppression and UI input correlation. Live rows must settle using
turn provenance or be retracted. The deterministic Playwright gate reconciles
DOM, `/api/state`, and graph nodes across reload and frame switches. The judged
runbook retains real-provider Phases 0–4 and owns semantic quality.
