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
cursor. A missed event requires an authoritative read. The future durable sink
and watermark work remains separate.

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

## Addendum: a tool call's row carries its call id and a tool-declared display (FIG-5290)

A host that renders tool calls from the row stream needs more than
`{operation, status}`: the call's identity, a summary of what it was asked,
and what it found. It must not get those from live activity or `TurnOutput`
(this ADR), and a host-side table keyed by tool hooks would be a second
projection with its own crash consistency. So the display is committed with
the call's outcome and rendered by the same fold.

**The display.** `ToolDisplay { arguments, result, links, truncated }` is an
optional argument summary, an optional result summary, and ordered citations
and links (`ToolDisplayLink { uri, title, description }`). It is not
`ToolViewBlock`: a view block carries inline attachment bytes, which no
durable record may hold, and an `f64` priority, which rows (`Eq`) cannot.
The link fields keep `ToolViewBlock::ResourceLink`'s names.

**Who declares it.** A tool returns it on its output
(`ToolCallOutput::with_display`), so it is journaled with the attempt that
produced it. A presentation step folds `PresentedToolReturn { model_return,
display }` and may replace or clear it, as it may change the model-facing
return (ADR 0128 §6: representation, never the result).

**Journaled once.** `PresentToolResult` records the folded display in
`ToolPresentation`, bounded to `TOOL_DISPLAY_LIMIT_BYTES` (16 KiB of
canonical JSON): links are dropped from the end, then the result and then the
argument summary are cut at a character boundary, and `truncated` records
the original and limit sizes. The completed call carries it in its committed
member outcome, so a replay or resume serves the journaled display and never
re-runs the tool or a step.

**Never model-facing.** No model path reads it. The standard protocol's
provider blocks are a result part's `content`; RLM's ledger, `Calls:` lines
and the `history` item project `ExecutedCallRecord {operation, outcome}`
only.

**Committed carriers.** The standard protocol's `ToolResult` part carries
the call's `status` and its `display`. An RLM trajectory entry's
`RlmExecutedCall` carries the call's `call_id` and `display` beside the
model-safe ledger pair; the `history` item drops both.

**Rows.** `RowTool { call_id, operation, status, display }`:

- RLM code blocks list one `RowTool` per ledger call, now with its call id
  and display.
- A standard message's `ToolCall` parts each add `RowTool { status:
  "requested" }`; its text still carries the model's raw request. The call
  node commits before its outcome exists, and the fold is per node, so the
  `Committed { rows }` a commit publishes equal an authoritative read. A
  host pairs the request row with its result row by `call_id`, a typed id,
  never a parsed one.
- A standard `ToolResult` part adds `RowTool { status, display }`. Its
  rendered result joins the row text only when the call declared no display.
  A message whose parts are tool results is a `ToolCall` row, not a `User`
  row.
- A call without a display keeps today's text. Its row gains the `RowTool`
  that names its call id.

A batch wrapper's folded result names the wrapper call. Its members'
displays are not carried.
