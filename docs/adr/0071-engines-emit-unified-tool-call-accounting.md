# Engines emit unified tool-call accounting outside model projection

## Context

Standard execution dispatches model-native tool calls. RLM executes TypeScript
that can dispatch tools inside an exec effect. Both contribute to the same
turn accounting, while each protocol owns the context it projects to the model.

## Decision

Each completed tool record in a successful RLM exec response contributes a
`SessionStreamEvent::ToolCall`, subject to the accounting bounds below. The
cell and native drivers emit these events in `handle_exec_result`, after
inspecting the full response for terminal tool control. Recorded exec responses
pass through this same handling path on redrive. Live trace and activity events
remain separate observations of execution.

Three rules apply:

1. Engines use the shared tool-call accounting vocabulary.
2. Terminal control uses the full payload before accounting truncation.
3. Protocol projectors decide which execution information reaches model context.

Tool-call accounting populates assembled turns and host observations. It does
not itself add conversation nodes or model attempts to the ADR 0032 ledger.

Each exec retains at most 128 tool records. Oversized inline string scalars,
above 64 KiB, become `omitted_bytes` markers recursively inside success and raw
failure values. Arrays, objects, and attachment references retain their shape.
The omitted tail contributes one `ToolCallsOmitted` event with its count,
failures, and attachment references. The final attachment scan includes both
retained records and that summary.

## Attachment commit acquires explicit referrers

The final turn transaction acquires a `Session` referrer on each committed
attachment id. The set includes stored references from tool outputs, the omitted
tool-call summary, message parts, and retained outputs. A code cell's recorded
response supplies retained prints and finish values that the commit cannot read
from protocol records directly. Acquisition validates the whole set and refuses
an attachment without upload evidence before installing any edge.

A put in a session runtime bound to an execution acquires an `Execution`
referrer. A put without that binding acquires an expiring `Upload` referrer.
Commit does not promote every put: an id kept only in opaque plugin state or
plain JSON is not part of the committed reference scan. An unreferenced turn
put loses its execution hold when the journal settles. Reclamation requires
the absence of both referrer edges and pending writes under
[ADR 0124](0124-attachments-are-kept-alive-only-by-their-referrers.md).

## Shared-history retention

Session deletion and explicit attachment deletion do not reclaim committed
attachment roots while retained history keeps their owner alive, including
another session's shared prefix. Session referrers retain these roots at
owner granularity under ADRs 0047 and 0124. Artifacts use the same referrer
vocabulary with their own edges and cleanup obligations under
[ADR 0113](0113-artifacts-are-kept-alive-only-by-their-referrers.md).

## Alternatives considered

Emitting accounting only at the live dispatch path loses reconstruction from a
recorded exec response. Treating accounting records as model history couples
host observation to a protocol's prompt. Truncating the terminal payload or
attachment references to satisfy accounting bounds changes execution or
retention, so those payloads are inspected or retained separately.

## Consequences

Both RLM channels report tool use through the common turn vocabulary. Bounded
host observations preserve terminal outcomes and attachment reachability.
Attachment and artifact edges remain separate store contracts within the shared
referrer vocabulary.

## Code references

- `crates/lash-protocol-rlm/src/protocol/driver.rs:495-558,888-1028` handles and bounds cell accounting.
- `crates/lash-protocol-rlm/src/native/driver.rs:368-428,648-788` does the same for native transport.
- `crates/lash-core/src/runtime/turn_boundary/recorded_assembly.rs:107-121` folds omitted summaries.
- `crates/lash-core/src/runtime/turn_boundary/materialize.rs:50-88` collects committed attachment ids.
- `crates/lash-core-store/src/attachments.rs:1665-1690` selects execution and upload referrers for puts.
- `crates/lash-sqlite-store/src/persistence/session_commit.rs:852-853` acquires session edges at commit.
- `crates/lash-sqlite-store/src/attachments.rs:80-137,850-860` validates acquisition and preserves retained roots.
- `crates/lash-core/src/runtime/artifact_cleanup.rs` executes artifact cleanup obligations.
