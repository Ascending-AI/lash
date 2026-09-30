# Engines emit unified tool-call accounting outside model projection

Lash has more than one execution engine. The standard engine asks the model for
native tool calls and dispatches them between model completions. The RLM engine
runs Lashlang, whose executor can dispatch tools internally before it returns an
`ExecResponse`. These are different control loops, but they are not different
kinds of tool use. Every completed call is part of the turn's accounting and
must therefore use the standard `SessionStreamEvent::ToolCall` event shape.

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

`SessionStreamEvent::ToolCall` is an accounting and host-observation event. It is not a
conversation or protocol graph node, and RLM trajectory entries remain free of
tool-call records. RLM emission consequently fills `AssembledTurn.tool_calls`,
host and remote turn summaries, and `TurnExecutionMetrics.had_tool_calls` without
changing the bytes projected back to the model. Exec calls continue to consume
no model tokens and add nothing to the ADR 0032 LLM-attempt ledger.

Accounting remains bounded without sacrificing attachment reachability. Large
inline `ToolValue` scalars are replaced in place by an `omitted_bytes` marker,
recursing through success values and failure or cancellation raw values while
preserving the surrounding arrays and objects. A fixed per-exec record cap (128)
keeps the first records and reports the tail as one typed
`SessionStreamEvent::ToolCallsOmitted` event whose `OmittedToolCalls` summary
carries the omitted `count`, `failures` and `attachments`. Attachment references
are small, load-bearing references rather than inline output bytes, so neither
bound may remove them. Tail attachments are carried by that summary and remain
visible to the ordinary tool-output attachment scan.

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
window. Explicit `SessionAttachmentStore::delete()` and session deletion
release eligible roots, while committed roots remain if retained history keeps
their owner alive. Attachment GC reclaims bytes only after the final root ends.

## Shared-history retention

The pending-retention description is historical. Shared history and attachment
roots are retained under ADRs 0028 and 0047: a session deletion or explicit
attachment delete does not reclaim committed roots while retained graph nodes
still keep them alive, including another session's shared prefix. The current
manifest keeps attachment roots conservatively at owner granularity, rather
than claiming an exact attachment-to-node edge for every reference.

[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md) governs
artifact retention through explicit referrer edges and durable cleanup
obligations. It is landed, not pending. The tool-call accounting and its
attachment-preservation bounds are unchanged.

## Amendment (FIG-4125, 2026-09-29)

Item 23: [ADR 0079](0079-one-promised-package-facade-owns-the-api.md) governs
the promised facade. The shared accounting rule survives for current tool
execution.

## Amendment (FIG-4163, 2026-09-30)

Shared-history retention is current, and ADR 0113's artifact referrers replace pending artifact-retention language without changing tool-call accounting.
[SQLite attachment root predicates](../../crates/lash-sqlite-store/src/attachments.rs)
and [the artifact cleanup executor](../../crates/lash-core/src/runtime/artifact_cleanup.rs)
enforce the separate attachment and artifact retention mechanisms.
