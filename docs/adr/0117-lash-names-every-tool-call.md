# 0117: Lash names every tool call

## Status

Accepted.

## Context

A provider's call id correlates a call with its response. It does not identify
one logical execution across admissions: providers may omit ids, repeat them,
or require a different spelling on another route. Tools need an idempotency
key that survives an unrecorded-effect crash and a reported-failure retry,
while distinguishing fresh calls that carry the same provider id.

[ADR 0042](0042-tool-attempts-are-atomic.md) and
[ADR 0110](0110-the-engine-owns-process-recovery.md) define the at-least-once
contract. Lash supplies the call identity; the external service supplies
idempotency in the window between an effect and the durable record of its result.

## Decision

### 1. Every admitted tool call has one `ToolCallId`

Every admitted logical tool call has a mandatory `ToolCallId`. The admission
root and source position determine it before execution. Crash recovery,
`Repeatable` ordinals and reported-failure retries preserve it. A fresh logical call gets a distinct id,
even when the model repeats the provider id.

Tools remain opaque and atomic, and each attempt follows its recorded
execution policy ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §7). A tool uses `call_id` as its
external idempotency key. If it needs per-attempt freshness, it combines the id
with `attempt_number`. Business deduplication across distinct logical calls
belongs to the service's domain key and fingerprint. Multi-effect durability
belongs to process steps, rather than a tool-body memo store or a body-level re-run opt-in.

### 2. The derivation

A `ToolCallId` is `tc_` followed by 64 lowercase hexadecimal digits. Its BLAKE3
domain is `lash-tool-call-id/v1`. Admitted derivation uses form tag 1; batch
extension uses form tag 2. Strings have a big-endian `u64` byte length, numbers
are big-endian `u64`s, and tags are one byte. The admitted form contains the
deployment namespace, root and counted position vector.

| Root tag | Root | Handle |
|---|---|---|
| 1 | Turn | Admitted turn handle |
| 2 | Host submission | Admitted submission handle |
| 3 | Process | Minted `ProcessId` |

Turn and submission handles refuse empty or whitespace-only strings with
`ToolCallRootError::BlankHandle`. A redelivery reuses its admission; a fresh
submission needs a fresh admission. The default deployment namespace is empty.

| Position tag | Position | Value |
|---|---|---|
| 1 | Continuation | Physical continuation |
| 2 | Iteration | Protocol iteration |
| 3 | Effect ordinal | Model response's durable effect ordinal |
| 4 | Content index | Full response index before filtering |
| 5 | Batch member | Original index, written only by `child` |
| 6 | Code opener | Canonical opener identity encoding |
| 7 | Code cell | Cell admission key |
| 8 | Code command | Whole-program issue ordinal |
| 9 | Code aggregate | Leaf's first-appearance index |

Tags are permanent. Equal numbers under different tags identify different
positions. `wrapper.child(original_member_index)` hashes form tag 2, the
wrapper's fixed-width digest spelling, and the tagged member index. Refusals
and completion order do not renumber members.

| Ingress | Identity inputs |
|---|---|
| Model call | Admitted run, continuation, iteration, effect ordinal, full content index |
| Batch member | Wrapper id and original member index |
| code mode cell command | Opener admission, code opener, cell admission key, command ordinal, and aggregate index for a leaf |
| Process-body command | Process admission, code opener, command ordinal, and aggregate index for a leaf |
| Host submission | Submission admission and any content position assigned by its caller |

`CodeCallIdentities` owns the code derivation shared by the Lash VM hosts and
worker broker. VM snapshots preserve the whole-program ordinal (ADR 0132 §8).
Provider ids, arguments, tool names, attempt numbers, scheduling order and
user labels are absent from the preimage.

Sources: `crates/lash-sansio/src/tool_call_id.rs`,
`crates/lash-sansio/src/sansio/turn_protocol.rs`, and
`crates/lash-vm-broker/src/identity.rs`.

### 3. The type

`lash_sansio::ToolCallId` has a private representation and no `Default` or
unchecked string constructor. `derive` and `child` mint ids; `parse`,
`FromStr`, `TryFrom<&str>` and `Deserialize` validate the `tc_` spelling.
`Display`, `Serialize` and the JSON schema use the plain string. Parsing
checks the spelling, not proof that a particular admission minted it.

The type lives beside the identity domain registry in lash-sansio.

Source: `crates/lash-sansio/src/tool_call_id.rs`.

### 4. When the identity becomes durable

The admission and the recorded plan precede the tool effect. A model call's
plan is its recorded response and full content order. A code command uses its
admitted issue position, committed with the VM snapshot; a host submission uses
its admission. Resuming the same plan yields the same ids. A fresh response has a fresh position even if
its provider ids match another response's.

Sources: `crates/lash-sansio/src/sansio/turn_protocol.rs`,
`crates/lash-core-execution/src/runtime/actor/round/records.rs`, and
`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs`.

### 5. The tool-facing API

The public `AttemptContext` exposes caller context (`owner`,
`enclosing_process`, `logical_run`, `process_spawn_provenance`) and:

```rust
fn call_id(&self) -> &ToolCallId;
fn attempt_number(&self) -> u32;
fn max_attempts(&self) -> u32;
fn intent_identity(&self, index: u32) -> ToolIntentIdentity;
fn completion_key(&self) -> Result<AwaitEventKey, RuntimeError>;
```

Its call id is mandatory. `completion_key` can refuse when the tool or host
lacks deferred-completion capability. The admitted internal `ToolContext`
stores the id and retry information and builds the attempt context.
Provider correlation belongs to protocol, transcript and display records,
rather than an accessor on the attempt context.

Source: `crates/lash-core-execution/src/tool_provider.rs`.

### 6. Derived keys

Tool addresses, activity ids, frames, environment, presentation, await and
cancellation keys derive from the call id. Attempts and retry sleeps add the
attempt number. Intent identities add the intent index; final-emission
attribution remains separate evidence. Completion keys are opaque bearer
capabilities. Hosts keep their association
with `call_id` in a durable record or read it through `Completions::parked`;
ADR 0137 owns that delivery contract.

Commit records retain both lash identity and provider correlation.
Run operand slots govern ordering independently of identity.

### 7. One record per call

A host tool call has one record under every protocol:
`ToolCallRecord { call_id, provider_call_id, tool, args, output }`, keyed by
its `call_id`. A protocol never keeps a parallel record of the call. What a
protocol knows beyond the record is an extra field on it or an extra record
that names it by `call_id`.

A code cell's executed calls are such records. Each `ExecutedCall` carries the
source `operation` the cell ran, its `outcome`, and `call_id:
Option<ToolCallId>`, the id of the host tool call the dispatch resolved to. It
is `None` only for a dispatch lash handled itself, with no host tool call. The
cell's result (`ExecResponse`) and its committed `CellRecord`, which the RLM
trajectory entry stores and the transcript returns, hold that one type, so a host joins its own
ledger, keyed on `AttemptContext::call_id()`, to the cell that made the call.
The model's view of a cell's calls (operation and outcome, no arguments and no
identity) is derived from those entries and is not stored.

A turn bounds the records it reports (`OmittedToolCalls` accounts for the
rest) and a cell entry keeps the tail of its calls (`calls_omitted` counts the
earlier ones), so an entry in a cell with more calls than either bound can
name a record the bounded view left out.

Sources: `crates/lash-sansio/src/session.rs` and
`crates/lash-core-store/src/transcript/mod.rs`. Law:
`an_rlm_cells_executed_calls_name_the_turns_tool_call_records_after_a_reopen`
(`crates/lash/src/tests/durable_session.rs`).

Sources: `crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs`,
`crates/lash-core-execution/src/tool_intent.rs`,
`crates/lash-core-store/src/await_event_identity.rs`, and
`crates/lash-sansio/src/frame_key.rs`.

A process tool step retains that same `ToolCallRecord` in its committed round
outcome. Its `ProcessEffectOccurrence::call_id` is the admitted typed id;
engine-only effects carry `None`. A host joins the occurrence to
`Processes::tool_call(process_id, call_id)`, which reads retained outcomes
without a live trace or current catalog. The engine receives the record's
output at incorporation. A parked call retains its request with its completion
metadata, so resolving it produces the same record after a reopen. Pruning the
process retires its tool records with its rounds. The actor also carries the
typed id in `EngineEvent::StepSettled`. The VM retains it with the leaf
settlement in its continuation input and binds its completion and failure
observations to that id when answering the reissued operation.

### 7. Retained payload drift is refused before any effect

Whole-round K1 admission records each call id, canonical request digest, prepared
payload, owner, capabilities and executable/preparation/presentation bindings.
Replay compares identity and content before fresh execution. The id is never
reminted on drift. Preparation is served from canonical A material and every
later attempt uses it. A changed request, missing binding or retained-material
failure refuses with its typed cause instead of running a new body (L12).

Source: `crates/lash-core-store/src/tool_run/admission.rs` and
`crates/lash-core-execution/src/tool_dispatch/production.rs`.

### 8. The provider boundary

The provider id is correlation. Shared response assembly repairs missing,
blank and within-response duplicate ids before admission. It reserves valid
ids first, then derives a noncolliding replacement from the request id, full
content index and collision counter. Streamed and non-streamed responses use
this path. Repetition in another response is valid. Repair preserves provider
round-trip metadata and item ids.

The LLM request carries paired call/result correlation strings. Each adapter
maps those strings together for its wire. Anthropic preserves legal ids and
maps other ids to a permitted prefix plus a hash suffix, checking request-wide
collisions. RLM enforces its one-`execute_code` grammar independently of lash
call identity.

Sources: `crates/lash-core/src/runtime/assembly.rs`,
`crates/lash-provider-anthropic/src/request.rs`, and
`crates/lash-protocol-rlm/src/native/tool.rs`.

### 9. Format ownership

Recorded keys are durable data under their format. The claim filter of
[ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md) §1 keeps a
node from claiming an actor whose recorded formats it cannot decode, so no
node reinterprets keys it does not read. A recorded key is never reminted to
fit a new derivation. Shapes change
in place during the pre-1.0 version freeze; upgrade read contracts belong to
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

Source: `crates/lash-sansio/src/tool_call_id.rs`.

### 11. Laws

`tool_call_identity_tests!` registers identity laws, including:

- `reported_failure_retry_preserves_call_id`;
- `repeated_provider_id_across_turns_is_distinct`;
- `same_scope_completion_collision`;
- `code_cells_keep_identity_and_distinguish_fresh_calls`.

`batch_admission_and_identity_contract` covers batch identity. Run admission
refuses retained-request drift (L12).
The store matrix is SQLite file, SQLite memory and PostgreSQL.  Laws run the production runtime over a fault-injecting store with labelled commits, a virtual clock and `SimNodes` ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §14). Upgrade proofs use the synthetic-next tier.

Sources: `crates/lash-durable-test/tests/tool_call_identity.rs`,
`crates/lash-durable-test/tests/tool_batches.rs`,
`crates/lash-core-store/src/tool_run/admission.rs`, and
`crates/lash-sim/src/invariants/tool_call_identity.rs`.

## Consequences

Retries preserve the external idempotency key. A service that caches a failure
needs a new logical call to try again. Provider correlation cannot alias lash
execution identity. Refusing malformed provider ids would discard usable
responses without improving tool idempotency; repairing correlation keeps the
provider boundary tolerant. A new id per retry would defeat deduplication
when an error follows a successful external write.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
