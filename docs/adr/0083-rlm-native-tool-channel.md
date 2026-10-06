# 0083: RLM channels are pinned when a session materializes

## Context

Cell transport and provider-native code calls execute against the same persistent
heap, catalog, checkpoints, and finish contract. Their model histories need
different projection and response-admission rules.

## Decision

`RlmProtocolPluginConfig.channel` explicitly selects `Cell` or `NativeTool`.
Materialization records that channel in durable protocol options. Rebuilding
requires the pin: substitution returns `RecordedSessionConfigConflict`, and
absence returns `MissingRecordedSessionConfig`. Channel selection does not infer
compatibility or translate one transport's history into another.

Native transport advertises one `execute_code` tool with exactly one required,
nonempty string property `code` and no additional properties. Tool choice is
auto. `finish` executes inside the program. More than one call executes nothing
and supplies repair text, consuming a stalled attempt. Unknown tools, invalid
JSON, missing code, and extra properties receive their own repair decisions.
Shared response assembly normalizes provider call ids under ADR 0117 before
native admission checks arity and arguments.

Both channels share execution, finish-schema validation, semantic trajectory,
control tools, bindings, and checkpoint identity. Drivers, projectors, response
normalization, and transport prompt copy are channel-specific. Native results
use the common observation renderer.

Ordered assistant parts and opaque replay metadata travel in `native_transport`
envelopes keyed by semantic step. The projector retains complete call/result
exchanges. Terminal suppression and failure scrubbing remove complete exchanges;
it cannot reconstruct provider signatures from semantic history. A semantic-only
frame seed is user context, rather than permission to invent provider calls.

Native parked-driver and transport data use their declared format guards and
fleet read windows. Durable formats follow ADRs 0106 and 0115; current version
identities live in the format registry.

A nonempty prose-only native response ends a Natural turn. FinishRequired
requests `finish`. Empty or reasoning-only native responses produce a provider
error. Finish values, schema mismatch, and execution errors use the common cell
adjudication contract. Completed code executions emit the shared cell-start and
cell-end observations.

2026-10-06 (FIG-5104): a Natural termination may state a finish schema
(`RlmTermination::Natural { schema }`, per send through
`allow_prose_or_finish_schema` or session-wide through the recorded
termination). Prose still ends the turn; a `finish` value is validated on both
channels like a FinishRequired one, and a mismatch takes the same path: a
Program cell failure carrying the mismatch, the schema-mismatch copy, and the
loop continues. A text schema is the chat shape: the finalization copy says
`finish` takes only the user-facing answer text, never a raw tool result, and
that prose is preferred.

## Alternatives considered

Treating native exchanges as ordinary cell text loses provider-owned transport
metadata. Reconstructing calls from semantic seeds invents replay authority.
Accepting serial or parallel multiple native code calls complicates the one
execution response contract; one code program can express the work explicitly.

## Consequences and non-goals

Both channels are selectable and durable. Native admission enforces one call
independently of the provider's parallel-call setting, which follows ADR 0121.
Live code streaming and serial multi-call execution are outside this contract.
Transport-specific history remains distinct from common execution state.

## Code references

- `crates/lash-protocol-rlm/src/plugin/channel.rs:36-78` records and validates the pin.
- `crates/lash-protocol-rlm/src/native/tool.rs:5-118` defines and admits the native ABI.
- `crates/lash-protocol-rlm/src/native/driver.rs` adjudicates responses and executions.
- `crates/lash-protocol-rlm/src/native/projector.rs` projects complete exchanges.
- `crates/lash-protocol-rlm/src/native/transport.rs` owns transport envelopes.
