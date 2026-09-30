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
`RlmChannel::NativeTool`. Materialization records `channel` in the durable protocol
turn options. Rematerialization requires that recorded pin
and refuses substitution with `RecordedSessionConfigConflict`; missing pins are
refused with `MissingRecordedSessionConfig`. There is no fallback or migration.
Hosts select the channel before opening sessions. Workbench uses
`LASH_RLM_CHANNEL=cell|native`; toolbench uses `--channel cell|native`.

The native ABI is one `execute_code` tool, auto choice, with exactly one required
string property `code` and no additional properties. `finish` remains inside the
program. More than one call executes nothing and returns identical repair text
for every distinct call id, consuming one no-progress attempt. Unknown tools,
invalid JSON and missing/empty code have distinct repair decisions. The separate
duplicate-id repair decision is historical, superseded by
[ADR 0117](0117-lash-names-every-tool-call.md): shared response assembly
normalizes missing, blank and duplicate provider ids before native admission.
Native normalization checks tool arity and schema on those normalized calls.

Execution, finish-schema validation, semantic trajectory, catalog, control tools,
bound variables and checkpoint identity are shared. Drivers, history projectors,
response normalization and transport prompts are separate implementations.
The existing cell observation renderer supplies the native tool-result bytes.
Native parked-driver state uses format version 2 after removing unused prose and
strictly refuses other versions; the shared executor snapshot format is unchanged. Ordered assistant
Parts, including provider ids and opaque replay metadata, live
in version-1 `native_transport` diagnostic envelopes keyed by semantic step id.
Transport decoding tolerates the original unstamped shape, refuses newer versions,
and surfaces malformed bindings as projection degradation without panicking. The native
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

Live code streaming and serial multi-call execution are non-goals. The original
absence of a parallel-call option is historical: provider generation options
expose `parallel_tool_calls`, governed by
[ADR 0121](0121-host-generation-settings-are-sent-or-refused.md).
Native arity is enforced by normalization independently of that option or
provider behavior. Semantic-only frame seeds
render as user context; they do not authorize reconstruction of provider calls.

## Amendment (FIG-4163, 2026-09-30)

ADR 0117 moves provider-id normalization upstream, and ADR 0121 governs the parallel-call generation option; the native single-code-call contract survives.
[Response assembly](../../crates/lash-core/src/runtime/assembly.rs),
[native normalization](../../crates/lash-protocol-rlm/src/native/tool.rs), and
[provider generation options](../../crates/lash-core-llm/src/provider/options.rs)
define these boundaries.
