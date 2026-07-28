# Engines emit unified tool-call accounting outside model projection

Lash has more than one execution engine. The standard engine asks the model for
native tool calls and dispatches them between model completions. The RLM engine
runs Lashlang, whose executor can dispatch tools internally before it returns an
`ExecResponse`. These are different control loops, but they are not different
kinds of tool use. Every completed call is part of the turn's accounting and
must therefore use the standard `SessionEvent::ToolCall` event shape.

The RLM driver emits one accounting event for each tool record returned by a
successful exec effect. It does so in `handle_exec_result`, after inspecting the
complete response for terminal tool control and before any terminal or
nonterminal branch returns. This is deliberately the response-handling seam,
not the live per-call execution seam. Live execution already emits trace and
activity start/completion pairs; emitting there again would duplicate those
channels. More importantly, a durable effect re-drive passes its recorded
`ExecResponse` through the same response-handling seam, so accounting and
attachment reachability are reconstructed after a cold restart.

The engine vocabulary carries three invariants:

1. Every engine emits standard tool-call accounting events.
2. Every engine's terminal outcome carries its full payload. Accounting bounds
   never determine or truncate the terminal control decision.
3. Engine internals reach model context only through that engine's projector.

`SessionEvent::ToolCall` is an accounting and host-observation event. It is not a
conversation or protocol graph node, and RLM trajectory entries remain free of
tool-call records. RLM emission consequently fills `AssembledTurn.tool_calls`,
host and remote turn summaries, and `ExecutionSummary.had_tool_calls` without
changing the bytes projected back to the model. Exec calls continue to consume
no model tokens and add nothing to the ADR 0032 LLM-attempt ledger.

Accounting remains bounded without sacrificing attachment reachability. Large
inline `ToolValue` scalars are replaced in place by an `omitted_bytes` marker,
recursing through success values and failure or cancellation raw values while
preserving the surrounding arrays and objects. A fixed per-exec record cap
collapses the tail into one marker carrying `omitted_records` and
`omitted_failures`; the marker is non-successful exactly when its omitted tail
contains a non-successful record. Attachment references are small, load-bearing
references rather than inline output bytes, so neither bound may remove them.
Tail attachments are carried by the overflow marker and remain visible to the
ordinary tool-output attachment scan.

## Why attachment commit combines owner promotion with explicit adoption

The final turn transaction has two complementary paths:

1. The store promotes every uncommitted manifest row bound to the durable
   `RuntimeTurnCommitStamp` turn id. This is the replay-safe backbone, including
   puts whose ids appear only in plain JSON or opaque plugin state. It needs no
   process-local set and reconstructs nothing from tool accounting.
2. Attachment references in typed tool outputs and message parts form the
   explicit adoption set. They preserve cross-turn and carried-in references;
   update-in-place deliberately no-ops when this session has no intent row.

Owner promotion intentionally commits attachment scratch created by a turn even
when core cannot inspect the opaque state that retains it. Failed or superseded
turns are not promoted: their uncommitted rows remain live through recovery and
become reclaimable only after durable supersession proof plus the retention
window. Explicit `SessionAttachmentStore::delete()` and session deletion still
drop references eagerly; normal attachment GC then reclaims unrooted bytes.

## Shared-history ruling (implementation pending)

The receipt-based GC oracle and eager per-session reference deletion above
describe the current implementation. ADR 0028 and ADR 0047 supersede those
mechanics as a ruling, not as shipped fact. The FIG-653 L7 retention work owns
the explicit attachment-edge relations and read-guard deletion.

When that layer lands, attachment liveness will derive from stored edges rather
than durable supersession proof plus receipt retention. Session deletion will
remove that session's roots and edges without reclaiming attachment edges still
reachable through another session's shared historical prefix. The tool-call
accounting ruling and its attachment-preservation bounds are unchanged.
