# ADR 0129: Committed history is typed facts a host renders

Status: Accepted

## Decision

Lash is an engine: it exposes committed conversation history as typed,
decoded facts, and it renders nothing. Hosts own every presentation decision:
row layout, labels, joined text, pretty-printed values and status words
(Sam, 2026-10-08, FIG-5430). Hosts do not choose an answer from
`TurnOutput`, inspect protocol payloads, classify committed parts, or
correlate messages by parsing an ID.

`lash::transcript` is a total fold of retained committed nodes. Every node
contributes exactly one `TranscriptEntry`: its opaque `EntryId`, its recorded
timestamp, its typed turn/input provenance, and a `TranscriptItem`:

- `Message`: a `TranscriptRole` and the message's `TranscriptBlock`s in part
  order (text, reasoning, attachments, tool calls with their arguments, tool
  results with their typed blocks, and code, output and error parts);
- `Cell`: a code cell a protocol executed, with its code, its prints (or
  their retained archive), its typed `CellResult` (completed, failed with its
  `CellFailure`, or finished with its `TerminalValue`), its executed calls,
  `calls_omitted` (the calls beyond the recorded bound) and its images;
- `Suppressed`: a named `SuppressionReason`.

Lash keeps the interpretation a host cannot do itself. Core owns node-backed
identity, timestamps, source order, provenance and the sealed reply: an entry
is its turn's reply exactly when the runtime minted the message's reply
marker (`provenance.is_turn_reply`), and a reply keeps only its marked prose
part. Core classifies a provider's user-role tool result as
`TranscriptRole::Tool`. A protocol's pure `TranscriptDecoderPlugin`
(`PluginFactory::transcript_decoder`) decodes the stored shapes it owns: the
RLM decoder turns its trajectory entries into cells and its reasoning into
assistant reasoning blocks. Decoders need no materialized plugin, so a durable
reader, a live commit's publication and a resident view decode alike.

FIG-972, FIG-5288: the UI owns its input rows. A UI correlates the opening
input by `provenance.turn_id` and every input by `provenance.input_id`, retaining
its own identity and attachments. The host computes the input ID before
sending, using `LashSession::input_id(&send_id)` or
`DurableSession::input_id(&send_id)`, and supplies that same send ID through
`SendBuilder::id`. Steering inputs can share a run's turn ID; their input IDs
identify them individually. A coalesced opening or terminal-withheld follow-on
commits one user message per admitted input, in admission order, with each
message naming exactly its own input. Its application evidence names that
message. Hosts do not derive correlation from either ID's spelling.

FIG-984: every settled turn has one committed reply. Its writer depends on
termination: standard output, an RLM finish, a terminal tool, or a stopped
turn's committed text. The writer supplies the existing sealed `TurnReply`;
the decoder consumes it. Host code never mints a reply marker.

`DurableSession::transcript()` walks retained ancestry across frame boundaries,
without restoring a writer or admitting a turn. `SessionReadView::transcript()`
decodes its captured committed graph. Raw read views remain available for
evidence, tools and auditing.

## Order and transport

A `SessionTranscript`'s entries are in source order; that order is not a
cursor. `EntryId` supports equality and transport without a public string
accessor. A `Committed { base_revision, entries }` observation carries only the
entries its commit added. It is a freshness event, not a complete-history
response or a durable change-feed cursor. A missed event requires an
authoritative read: the committed-turn read below.

## Committed-turn read (FIG-5297)

A host that post-processes a conversation (memory extraction, analytics,
indexing, sync to another store) reads the session's committed turns after a
cursor it keeps: `DurableSession::committed_turns(after, limit)` returns
`CommittedTurnsPage { turns, next }`, oldest first by commit. Each
`CommittedTurn` carries the turn id, its committed entries, its commit time
and its typed `TurnCommitOutcome`. Its entries decode exactly the nodes the
turn's commit appended, every admitted input's user message among them
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
turn's entries; the turn is still served. There is no wait variant: hosts
poll.

A host can retain canonical records in its own storage, in its own shape.
Agent-service's SQL mirror and Slack's pending/post records are such sinks.

## Enforcement

Compile-fail fixtures seal committed classification, reply construction,
entry identity and the protocol decoder. Exhaustive in-crate matches name
every suppression and role.

`scripts/check_transcript_projection.py` independently scrapes committed-truth
reads and every turn-output accessor call. The reviewed registry pins their
counts and dispositions, pins the decoded item and role vocabulary every
rendering surface handles, rejects host classification and ID parsing, and
ties every rendered asset to the shared production-JavaScript harness.

The typed-history law (`committed_turns.rs`) renders a Standard turn's tool
call, result and sealed reply and an RLM turn's cells (one with omitted calls)
and sealed reply from typed entries alone, live from the observation feed and
after recovery, on every store tier, and requires both renderings to agree.
The workbench renders its own rows from these entries; its Playwright gate
reconciles DOM, `/api/state`, and graph nodes across reload and frame
switches.
