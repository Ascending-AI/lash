# 0083 — RLM channels are pinned when a session materializes

Date: 2026-09-08

## Status

Accepted

## Context

[FIG-2164](https://linear.app/ascending-ai/issue/FIG-2164) compares provider-native
code calls with paired dialect cells. Both execute against the same persistent
heap, tool catalog, execution checkpoint and finish contract. Their transport
histories require different ownership and projection rules.

## Decision

`RlmProtocolPluginConfig.channel` selects `RlmChannel::Cell` or
`RlmChannel::NativeTool`. Materialization records `channel` in the durable session
options, alongside the dialect. Rematerialization requires that recorded pin
and refuses substitution with `RecordedSessionConfigConflict`; missing pins are
refused with `MissingRecordedSessionConfig`. There is no fallback or migration.
Hosts select the channel before opening sessions. Workbench uses
`LASH_RLM_CHANNEL=cell|native`; toolbench uses `--channel cell|native`.

The native ABI is one `execute_code` tool, auto choice, with exactly one required
string property `code` and no additional properties. `finish` remains inside the
program. More than one call executes nothing and returns identical repair text
for every distinct call id, consuming one no-progress attempt. Unknown tools,
invalid JSON, missing/empty code and duplicate ids have distinct repair decisions.
Duplicate ids cannot form a valid provider exchange: all original Parts remain
recorded, but projection emits one error pair per distinct id.

Execution, finish-schema validation, semantic trajectory, catalog, control tools,
bound variables and checkpoint identity are shared. Drivers, history projectors,
response normalization and transport prompts are separate implementations.
The existing cell observation renderer supplies the native tool-result bytes.
Native parked-driver state starts at format version 1 and strictly refuses other
versions; the shared executor snapshot format is unchanged. Ordered assistant
Parts, including provider ids and opaque replay metadata, live
in `native_transport` diagnostic envelopes keyed by semantic step id. The native
projector emits each call and its result together; terminal suppression and
failure scrubbing remove complete exchanges. Provider signatures are never
reconstructed. Existing `lashlang:` and `lashlang_step_*` durable identities stay.

A nonempty prose-only response ends a Natural turn. FinishRequired requests
`finish`; finish values, schema mismatch and execution errors follow the cell
adjudication contract. Empty native responses, including reasoning-only, stop
with ProviderError. The frozen cell driver currently adjudicates nonempty
reasoning-only responses: parity tests cover truly empty responses, and a
native-only test pins the clarified reasoning-only rule. Changing that cell
behavior requires a separate contract change.

Completed native code calls emit the dialect's existing cell-start and cell-end
runtime events once. No argument deltas or stream-mask hooks are installed.

## Consequences and non-goals

Both channels remain first-class pending measurement. Toolbench pairs identical
model strings, route, dialect and budgets in randomized order, preflights native
support, and records per-attempt decisions, billed retry/cache usage and timings.
The complete benchmark and recommendation are follow-ups.

Live code streaming and serial multi-call execution are non-goals. `LlmRequest`
has no parallel-tool-call flag; this change does not add one. Arity is enforced
by normalization, independently of provider behavior. Semantic-only frame seeds
render as user context; they do not authorize reconstruction of provider calls.
