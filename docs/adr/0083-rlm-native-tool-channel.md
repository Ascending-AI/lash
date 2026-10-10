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
auto. `control.finish` is a declared control tool the program calls
(FIG-5781). More than one call executes nothing
and supplies repair text, consuming a stalled attempt. Unknown tools, invalid
JSON, missing code, and extra properties receive their own repair decisions.
Shared response assembly normalizes provider call ids under ADR 0117 before
native admission checks arity and arguments.

Both channels share execution, finish-value settlement, semantic trajectory,
control tools, bindings, and checkpoint identity. Drivers, projectors, response
normalization, and transport prompt copy are channel-specific. Native results
use the common observation renderer.

Ordered assistant parts and opaque replay metadata are ordinary committed
history (FIG-5527): the reply that ran a cell is an assistant message whose
`MessageOrigin::TurnOutput` names the cell in `cell_id`, the `CellRecord::id` of
its trajectory entry, as the cell channel's assistant message does. A reply
whose calls ran nothing is an assistant message followed by one tool result per
call carrying the correction. The projector retains complete call/result
exchanges. Terminal suppression and failure scrubbing remove complete exchanges;
it cannot reconstruct provider signatures from semantic history. A semantic-only
frame seed is user context, rather than permission to invent provider calls.

Native parked-driver data uses its declared format guard and fleet read
window. Durable formats follow ADRs 0106 and 0115; current version
identities live in the format registry.

A nonempty prose-only native response ends a Natural turn. TerminalRequired
requests `control.finish`, and a TerminalRequired turn whose tool surface
declares no `Finish` is refused before its model is called (FIG-5781). Empty or reasoning-only native responses produce a provider
error. Finish values, schema mismatch, and execution errors use the common cell
adjudication contract. Completed code executions emit the shared cell-start and
cell-end observations.

A turn's final value has the type of the finish tool that ended it
(FIG-5823). Each tool that declares `Finish` states its value schema on the
declaration, and settlement checks every Finish value against the schema
the call was admitted under, on both channels. A mismatch fails the call
with no completion candidate and is never repeated; the model reads it and
the loop continues. A host that wants a typed answer offers its own finish
tool, which takes the place of `control.finish`; a send that offers it
retyped through its tool access types that run alone. Termination is the
session's, fixed at creation. Under Natural, prose still ends the turn, and
the finalization copy names the finish tools the surface offers.

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
