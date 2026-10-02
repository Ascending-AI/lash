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
root and source position determine it before execution. Crash replay and
reported-failure retries preserve it. A fresh logical call gets a distinct id,
even when the model repeats the provider id.

Tools remain opaque, atomic and at-least-once. A tool uses `call_id` as its
external idempotency key. If it needs per-attempt freshness, it combines the id
with `attempt_number`. Business deduplication across distinct logical calls
belongs to the service's domain key and fingerprint. Multi-effect durability
belongs to process steps, rather than a tool-body memo store or replay opt-in.

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
| 7 | Code cell | Cell replay key |
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
| RLM cell command | Opener admission, code opener, cell replay key, command ordinal, and aggregate index for a leaf |
| Process-body command | Process admission, code opener, command ordinal, and aggregate index for a leaf |
| Host submission | Submission admission and any content position assigned by its caller |
| Trigger delivery | The bound process admission and that process's command positions |

`CodeCallIdentities` owns the code derivation shared by the Lashlang hosts and
worker broker. Process segment boundaries preserve the whole-program ordinal.
Provider ids, arguments, tool names, attempt numbers, scheduling order and
user labels are absent from the preimage.

Sources: `crates/lash-sansio/src/tool_call_id.rs:19`,
`crates/lash-sansio/src/sansio/turn_protocol.rs:897`, and
`crates/lash-vm-broker/src/identity.rs:86`.

### 3. The type

`lash_sansio::ToolCallId` has a private representation and no `Default` or
unchecked string constructor. `derive` and `child` mint ids; `parse`,
`FromStr`, `TryFrom<&str>` and `Deserialize` validate the `tc_` spelling.
`Display`, `Serialize` and the JSON schema use the plain string. Parsing
checks the spelling, not proof that a particular admission minted it.

The type lives beside the identity domain registry in lash-sansio.

Source: `crates/lash-sansio/src/tool_call_id.rs:243`.

### 4. When the identity becomes durable

The admission and the recorded plan precede the tool effect. A model call's
plan is its recorded response and full content order. A code command uses its
journaled issue position; a host submission uses its admission. Replaying the
same plan yields the same ids. A fresh response has a fresh position even if
its provider ids match another response's.

Sources: `crates/lash-sansio/src/sansio/turn_protocol.rs:846`,
`crates/lash-core-execution/src/session/tool_execution/group.rs:294`, and
`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs:42`.

### 5. The tool-facing API

The public `AttemptContext` exposes:

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

Source: `crates/lash-core-execution/src/tool_provider.rs:372`.

### 6. Derived keys

Tool addresses, activity ids, frames, environment, presentation, await and
cancellation keys derive from the call id. Attempts and retry sleeps add the
attempt number. Intent identities add the intent index; final-emission
attribution remains separate evidence. Completion keys include the execution
scope. Commit records retain both lash identity and provider correlation.
Group slots govern ordering independently of identity.

Sources: `crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs:42`,
`crates/lash-core-execution/src/tool_intent.rs:401`,
`crates/lash-core-store/src/await_event_identity.rs:1`, and
`crates/lash-sansio/src/frame_key.rs:39`.

### 7. Retained payload drift is refused before any effect

A tool-child group's formation journals one `{group}:requests` record. Each
tool entry contains its call id, a digest of canonical tool identity, name,
arguments and authority, and its prepared payload. A replay compares the id
and digest before execution. A mismatch is `LashlangCellBindingDrift`; the id
is not reminted. The prepared payload is retained and served, rather than
compared with fresh preparation. Every later attempt executes the retained
payload. One record keeps formation replay bounded independently of width.

Source: `crates/lash-core-execution/src/session/tool_execution/group.rs:416`.

### 8. The provider boundary

The provider id is correlation. Shared response assembly repairs missing,
blank and within-response duplicate ids before admission. It reserves valid
ids first, then derives a noncolliding replacement from the request id, full
content index and collision counter. Streamed and non-streamed responses use
this path. Repetition in another response is valid. Repair preserves provider
replay metadata and item ids.

The LLM request carries paired call/result correlation strings. Each adapter
maps those strings together for its wire. Anthropic preserves legal ids and
maps other ids to a permitted prefix plus a hash suffix, checking request-wide
collisions. RLM enforces its one-`execute_code` grammar independently of lash
call identity.

Sources: `crates/lash-core/src/runtime/assembly.rs:596`,
`crates/lash-provider-anthropic/src/request.rs:707`, and
`crates/lash-protocol-rlm/src/native/tool.rs:121`.

### 9. Generation ownership

Executable records remain owned by their admitted generation. The generation
and admission fence of [ADR 0106](0106-durable-formats-upgrade-by-migration-or-drain.md)
prevents a replica from reinterpreting another generation's recorded keys.
An executable record is never reminted to fit a new derivation. Shapes change
in place during the pre-1.0 version freeze; upgrade read contracts belong to
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).

Sources: `crates/lash-core/src/runtime/turn_loop/generation_fence.rs:25` and
`crates/lash-restate/src/process/workflow.rs:1210`.

### 11. Laws

`tool_call_identity_tests!` registers identity laws, including:

- `tool_identity_survives_unrecorded_effect_crash`;
- `reported_failure_retry_preserves_call_id`;
- `recorded_outcome_skips_execution`;
- `repeated_provider_id_across_turns_is_distinct`;
- `same_scope_completion_collision`.

The conformance laws also cover batch identity and retained-request drift.
The store matrix is SQLite file, SQLite memory and PostgreSQL. Host coverage
uses the in-process Restate server double, live Restate and lash-sim's
in-process effect host. Upgrade proofs use the synthetic-next tier.

Sources: `crates/lash-conformance/src/macros/tool_call_identity.rs:1`,
`crates/lash-conformance/src/conformance/tool_call_identity/drift.rs:1`, and
`crates/lash-sim/src/invariants/tool_call_identity.rs:1`.

## Consequences

Retries preserve the external idempotency key. A service that caches a failure
needs a new logical call to try again. Provider correlation cannot alias lash
execution identity. Refusing malformed provider ids would discard usable
responses without improving tool idempotency; repairing correlation keeps the
provider boundary tolerant. A new id per retry would defeat deduplication
when an error follows a successful external write.
