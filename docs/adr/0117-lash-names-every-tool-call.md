# 0117: Lash names every tool call

## Status

Accepted 2026-09-29 (FIG-4073). FIG-4077 lands this ADR and the
`ToolCallId` type (§3, §11); the lanes of §11 build the rest.

Amends [ADR 0042](0042-tool-attempts-are-atomic.md) (the idempotency key of
its at-least-once rule), [ADR 0110](0110-the-engine-owns-process-recovery.md)
§3 (the same key) and [ADR 0116](0116-tools-are-opaque.md) §1.3 (the identity
reads on `AttemptContext`) and §2.2 (member call ids). The at-least-once
contract of ADR 0042 and ADR 0110 stands unchanged.

The rulings on FIG-4073 (Sam, 2026-09-29) are binding:

1. **At-least-once stays the tool contract.** There is no exactly-once, no
   replay opt-in, no tool-body memo store and no action ledger.
2. **The key tools use is lash's, never the provider's.**
3. **The provider boundary repairs malformed ids and never refuses them.**
   This reverses the stricter boundary the round-2 study proposed (its
   "Orchestrator amendment").
4. **The version freeze (FIG-3846).** Shapes change in place, with no version
   bumps, adapters or shims.

The design inputs are in `/workspace/notes/lash/tasks/lanes/`:
`study-4073.report.md` (round 1: the identifier inventory and the
derivation) and `study-4073b.report.md` (round 2: the comparison with pi's
harness, the final API, the deletion inventory and the laws, and at its end
the orchestrator amendment that reverses its provider-boundary strictness).
Where they conflict, the rulings win, then the amendment, then round 2.
Every citation below was checked at `7baa6152ee`.

## Context

ADR 0042 and ADR 0110 §3 make tools at-least-once: the engine replays
recorded outcomes, and the one window no journal covers, an effect that ran
without its result being recorded, is the implementor's to make safe "by
idempotency keyed by lash's stable call id". Lash has no such id. What a tool
sees as its call id is the provider's.

- **The key is the provider's string.** `PendingToolCall.call_id` is a
  string (`crates/lash-sansio/src/sansio/turn_protocol.rs:16`), copied into
  `PreparedToolCall` (`crates/lash-core-execution/src/tool_provider.rs:1370`);
  a second constructor takes arbitrary caller strings (`:1383`). The
  tool-facing `tool_call_id()` and `replay_key()` are optional on both
  contexts (`:335,361,1103,1139`), and `with_retry_context` (`:1241-1253`)
  builds `lash-tool:{session}:{call_id}:{tool_name}`, with no turn, frame or
  attempt in it.
- **Providers repeat ids.** Standard responses accept repeated ids across
  turns, and RLM's duplicate check covers one response only
  (`crates/lash-protocol-rlm/src/native/tool.rs:42`). Local models emit
  `call_0` and `functions.name:0` (vLLM's Kimi parser takes the model's text
  verbatim). Two turns that both say `call_0` present one idempotency key to
  the tool, so a service that deduplicates on it drops the second call.
- **Within one execution scope the collision reaches lash's own keys.** Equal
  provider ids give equal completion ids
  (`crates/lash-core-execution/src/tool_dispatch/attempt_coordinator.rs:281-286`,
  `crates/lash-core-effect/src/await_event_identity.rs:51`) and equal env,
  presentation and await addresses
  (`crates/lash-core-execution/src/runtime/effect/tool_child_driver.rs:1223,1648,1735`).
  An unrelated deferred call can consume the earlier one's resolution.
- **Providerless calls have no identity of their own.** Direct dispatch
  falls back to `tool:{session}:{name}`
  (`crates/lash-core-execution/src/tool_dispatch/preparation.rs:44-48`), host
  invocations take whatever string the host supplies
  (`crates/lash-core-execution/src/session/tool_execution.rs:149`), and batch,
  command, process and language call ids are four more formulas
  (`attempt_coordinator.rs:28-105`,
  `crates/lash-lashlang-runtime/src/host_identity.rs:96-104`).
- **Adapters repair ids inconsistently.** OpenAI chat and responses mint
  random UUIDs for absent ids and, when streaming, for empty ones
  (`crates/lash-provider-openai/src/chat.rs:622,1133,1195`,
  `responses_shared.rs:433,1471`); Google does the same
  (`crates/lash-provider-google/src/stream.rs:161`). A random repair is not
  replay-stable. Anthropic's outbound normaliser truncates at 64 characters
  (`crates/lash-provider-anthropic/src/request.rs:700-718`), so two distinct
  ids can normalise to one.

Pi's harness (upstream `4259686`) reserves one result-entry id per
full-content position, commits it with the assistant entry before the tool
runs, and passes the raw provider id to `execute`. It adds a replay opt-in
and per-invocation memos on top. The ruling keeps pi's discipline (the
identity is durable before the effect) and rejects its opt-ins: replay
`never` is the at-most-once marker ADR 0110 §3 rules out, and body-side memo
writes would reverse ADR 0042 and ADR 0116's controller-free body.

Pi also shows why the boundary must stay lenient. Real providers send
missing ids (OpenAI-compatible completions and Bedrock pair by stream index),
duplicate ids (Google, which pi re-mints), ids another provider cannot accept
(Mistral's nine characters; Anthropic's charset and 64-character limit), and
calls whose result is orphaned. Once no lash identity derives from the
provider id, refusing any of these would break a real provider for no gain.

## Decision

### 1. Every admitted tool call has one `ToolCallId`

Lash assigns every admitted logical tool call one mandatory `ToolCallId`,
derived from its durable admission and its source position, and recorded
before execution. Crash replay and reported-failure retries preserve it;
distinct calls receive distinct ids. Provider call ids are correlation only.
Contexts expose `call_id` and a separate `attempt_number`; every
tool-derived identity is derived from `call_id`. A retained payload that
conflicts with its identity fails before any effect. Tools remain opaque,
atomic and at-least-once. External services own idempotency in the
unrecorded-effect window, keyed by `call_id`. Multi-effect durability belongs
to process steps; tool-body memos, replay opt-ins and domain-action ledgers
are excluded. Shapes change in place without compatibility paths, and old
generations drain before the cutover (§8).

A reported failure keeps the id. A timeout or error can follow a successful
external write, so a new key per attempt would defeat the service's
deduplication. A tool that wants per-attempt freshness combines `call_id`
with `attempt_number` itself. A fresh logical call, such as the model
reissuing a call in a later response, sits at a fresh position and gets a
fresh id.

Deduplicating one business operation across distinct logical calls is the
service's job, with its own domain key and fingerprint. A lash call id
deliberately tells those calls apart.

### 2. The derivation

A `ToolCallId` is `tc_` followed by the 64 lowercase hex digits of a BLAKE3
digest under the registered domain `lash-tool-call-id/v1`. The preimage is
length-delimited throughout: every string is a big-endian `u64` length and its
bytes, every number a big-endian `u64`, every tag one byte, and the position
vector is preceded by its count. Its inputs are:

- **The deployment namespace** (ADR 0111; empty for the default namespace).
  It is part of a deployment's identity, not a display label: moving a
  deployment to another namespace makes it a new deployment, and restored
  copies of one deployment never execute one namespace and root
  independently.
- **The durable admission root**, tagged by kind:

  | Tag | Root | Handle |
  |---|---|---|
  | 1 | turn | the turn's admitted operation handle |
  | 2 | host submission | the submission's admitted operation handle |
  | 3 | process | the minted `ProcessId` (ADR 0107) |

  A blank handle roots nothing (`ToolCallRootError::BlankHandle`). A host
  redelivery presents its admitted handle again; a new submission is
  admitted under a new one.
- **The tagged position vector**, locating the call inside its root:

  | Tag | Position | Value |
  |---|---|---|
  | 1 | continuation | the turn's physical continuation |
  | 2 | iteration | the protocol iteration |
  | 3 | effect ordinal | the durable effect ordinal of the model response |
  | 4 | content index | the call's full original content index, before filtering |
  | 5 | batch member | the original member index; written only by `child` |
  | 6 | code opener | the admitted opener's canonical identity encoding |
  | 7 | code cell | the cell's replay key inside its turn |
  | 8 | code command | the whole-program command ordinal |
  | 9 | code aggregate | a leaf's first-appearance index in its aggregate |

  Tags are permanent; a retired tag stays burned. The same number under two
  tags names two positions.

A batch member's id is `wrapper.child(member_index)`: a second preimage form
over the wrapper's digest and tag 5. It is the only spelling of a member, so
there is no second formula to disagree with it.

Per ingress:

| Ingress | Root | Positions |
|---|---|---|
| Model call | turn | continuation, iteration, effect ordinal, content index |
| Batch member | the wrapper's id | `child(original member index)`, counted before refusals |
| RLM cell command | turn | the executing call's model positions, then code opener, cell, command, and aggregate for a leaf |
| Process-body command | process | code opener, command, and aggregate for a leaf |
| Host submission | host submission | none, or content index when one submission carries several calls |
| `ProcessInput::ToolCall` | process | none |
| Trigger delivery | the registered process it is bound to, not its subscription template (`crates/lash-core-execution/src/triggers/router.rs:624-653`) | as the process's own calls |

Process segment boundaries never reset code positions, matching
`host_identity.rs:25-104`. `ProcessInput::ToolCall` stays (ADR 0116 §9); its
admitted process root names the call.

**Excluded** from every preimage: the provider call id, the arguments, the
tool name, the attempt number, scheduling and completion order, and any user
label. Uniqueness rests on admitted roots and the collision-resistant
encoding, never on a provider's guarantee.

### 3. The type

`lash_sansio::ToolCallId` (`crates/lash-sansio/src/tool_call_id.rs`) is
sealed. There is no construction from an arbitrary string and no `Default`:

```rust
impl ToolCallId {
    pub fn derive(namespace: &str, root: ToolCallRoot<'_>, positions: &[ToolCallPosition<'_>]) -> Self;
    pub fn child(&self, member_index: u64) -> Self;
    pub fn parse(value: &str) -> Result<Self, InvalidToolCallId>;
    pub fn as_str(&self) -> &str;
}
// ToolCallRoot::{turn, host_submission} -> Result<_, ToolCallRootError>; ToolCallRoot::process
// ToolCallPosition::{Continuation, Iteration, EffectOrdinal, ContentIndex,
//                    CodeOpener, CodeCell, CodeCommand, CodeAggregate}
```

`Display`, `Serialize` and the JSON schema are the plain `tc_…` string;
`FromStr`, `TryFrom<&str>` and `Deserialize` validate it and refuse every
other string, provider ids included. It lives in lash-sansio because that is
the lowest crate holding the BLAKE3 domain registry
(`core_support.rs`), beside `FrameKey` and `ProcessId`; lash-core-effect
depends on it.

### 4. When the identity becomes durable

Two facts are durable before any tool effect: the root is admitted, and the
call plan that places the call at its positions is recorded. For a model
call the plan is the recorded response with its content order; for a code
command, the journaled issue ordinal; for a host submission, its admission.
Dispatch reads the id from the recorded plan and never recomputes it from
live state. A recorded model response replays to the same calls; a new
response with the same provider ids is a new effect ordinal and new ids.

### 5. The tool-facing API

Both the public `AttemptContext` and the crate-private admitted `ToolContext`
expose:

```rust
fn call_id(&self) -> &ToolCallId;
fn attempt_number(&self) -> u32;
fn max_attempts(&self) -> u32;
fn intent_identity(&self, index: u32) -> ToolIntentIdentity;
fn completion_key(&self) -> Result<AwaitEventKey, RuntimeError>;
```

`call_id` is not optional. `completion_key`'s error means the host lacks the
capability, never that identity is missing. Unadmitted dispatch
configuration is not a `ToolContext`. There is no provider-id accessor on
either context: `provider_call_id` lives on protocol, transcript and display
records only, and a provider-backed record requires it. The optional
`tool_call_id()` and `replay_key()` accessors are deleted. The other
controller-free capabilities of ADR 0116 §1.3 are unchanged.

### 6. Derived keys

Every tool-derived identity is derived from the `ToolCallId`:

- group-child addresses, env, presentation, await and cancellation keys,
  activity ids and frame keys: from the call id;
- attempt and retry-sleep keys: the call id plus `attempt_number`;
- intent identity: the call id plus the intent index. Final-emission
  attribution stays separate fencing evidence (ADR 0042);
- completion: the call id within its execution scope, so two calls sharing a
  provider id never share a completion;
- commit identity: the call id and the preserved provider correlation, both.

Group slots still govern ordering. Unrelated effect addresses and the
`StartKey` namespaces of ADR 0107 are unchanged; a declared start stays keyed
by its intent (namespace 1), whose identity now roots in the call id.

### 7. Retained payload drift is refused before any effect

The canonical tool name, arguments, prepared payload and authority are bound
to the retained identity when the call is recorded. A replay or redrive
whose retained request differs under the same id is refused before
execution, through the existing drift refusal (ADR 0116 §2.2). No identity
is reminted to make it fit.

### 8. The provider boundary

The provider id is correlation. The boundary repairs it and never refuses it.

- **Inbound.** A missing, blank or within-one-response duplicate provider id
  gets a correlation id, once, in the shared stream accumulator
  (`crates/lash-core/src/runtime/assembly.rs`), after assembly and before
  admission. The repair is deterministic in the call's position in the
  assembled response, so replaying a recorded response yields the same ids.
  Non-stream responses take the same path. Ids repeated in a later response
  are valid and untouched. The per-adapter random UUID repairs are deleted.
  Provider replay metadata and item ids are preserved.
- **Outbound.** Per-provider normalisation stays: Anthropic's charset and
  64-character limit, split call and item ids, and any other target rule.
  Calls and results pair by `ToolCallId`, never by raw provider string, and
  are mapped together. Legal ids pass through unchanged. When normalisation
  would make two distinct ids collide within one request, the adapter
  re-qualifies them deterministically with a hash suffix and rechecks the
  result. Truncation alone is never the mapping, and a collision is never a
  fault. Legitimately repeated ids in separate messages stay repeated unless
  the target requires request-wide uniqueness, in which case each occurrence
  is mapped by its `ToolCallId`.
- RLM's duplicate-id branch (`native/tool.rs:51-56`) stays only as protocol
  UX, asking the model for one `execute_code` call. It carries no identity.

### 9. Old generations drain before the cutover

Old journal keys cannot be reinterpreted under the new derivation. Old
invocations drain on their own generation; the existing generation and
admission fence (ADR 0106) stops a new-generation replica from looking up a
new key for an old record. An old executable record is never reminted.
There is no migration and no dual-read path.

### 10. Deletion inventory

The full file:line list is the "Complete replacement/deletion inventory" of
`study-4073b.report.md`, as amended. In summary:

- the optional context ids and replay keys, their builder defaults and the
  `lash-tool:` synthesis; the string-taking prepared constructors and host
  `ToolInvocation::new`; the missing-id-only refusals
  (`tool_result.rs:162-170`, `crates/lash-core-store/src/runtime_error.rs:388`,
  `ToolIntentRefusalReason::MissingToolCallId`);
- the competing Scalar, Batch, Command and Process identity formulas in
  `attempt_coordinator.rs:28-105`, its raw completion derivation, the
  `preparation.rs` fallback, the batch suffixes (`tool_provider.rs:1434,1451`),
  the content-derived batch identity (`session/tool_execution.rs:438-491`),
  the language call renderers (`host_identity.rs:96-104`) and ADR 0116 §2.2's
  `{wrapper call id}/batch/{member index}`;
- the raw-id env, presentation, await, cancellation, child, intent
  (`tool_intent.rs:369-441`), frame (`frame_key.rs:43`) and completion
  (`await_event_identity.rs:51`) derivations, each replaced by §6;
- the conflated pending, completed and transcript ids, commit encoding,
  retained envelopes, settlements, process inputs and remote and trace
  projections, which carry `ToolCallId` plus provider correlation; raw-string
  and `None` fixtures, obsolete accessor docs, and schemas and goldens
  regenerated in place;
- the provider UUID repairs and Anthropic's truncating normaliser, replaced
  by §8. The round-2 report's deletion of RLM's duplicate-id branch is
  withdrawn by the amendment.

Every replaced adapter, dual path and flag goes. Nothing is version-bumped.

### 11. Laws and lanes

The laws, on the in-process, Restate double and live tiers:

- `tool_identity_survives_unrecorded_effect_crash`;
- `reported_failure_retry_preserves_call_id`;
- `recorded_outcome_skips_execution`;
- `repeated_provider_id_across_turns_is_distinct`;
- `same_scope_completion_collision`: two calls sharing a provider id in one
  execution scope never consume each other's completion;
- batch refusals and parallel completion never renumber identity;
- code segments, compaction and frames keep identity on replay and
  distinguish fresh calls;
- host, process and trigger admission survive replay;
- retained payload drift is refused before effects;
- every adapter repairs malformed ids deterministically and never lets
  normalisation collide, preserving echo;
- a non-final `DeclaredStart` launches nothing.

`batch_admission_and_identity_contract`, `batch_replay_preserves_fold_and_ranks`,
`batch_redrive_reuses_children` and `declared_start_discarded_retry_launches_nothing`
(ADR 0116 §7) are retained. Race laws run 20 repetitions; the live laws pass
five consecutive runs of `just effect-group-conformance-e2e`.

| Lane | Ticket | Scope | Edges |
|---|---|---|---|
| Seam | FIG-4077 | this ADR, its amendments, and the `ToolCallId` type with no callers | none |
| Provider boundary | FIG-4078 | §8: the accumulator repair, deletion of the adapter UUID repairs, collision-safe outbound normalisation | SOFT after FIG-4077 |
| Conformance laws | FIG-4079 | the laws above, on branch `fig-4073/identity`, red on `main` where they fail today | SOFT after FIG-4077; gates FIG-4080's landing |
| Core cutover | FIG-4080 | §4 to §7, §9 and §10's inventory outside the provider crates; integrates FIG-4079 and lands once | **HARD** after ADR 0116 lane D (FIG-4054), which rewrites the same contexts, dispatch and child drivers |

## Consequences

- A tool's idempotency key is lash's, mandatory, and the same on every replay
  and retry of one logical call. A model that repeats `call_0` across turns,
  or within one scope, aliases nothing.
- Providers keep working as they do today, malformed ids included; their
  repairs become replay-stable and live in one place.
- Retries keep their key, so a service that cached a failure needs a new
  logical call to try again.
- Tools get no memo store, replay opt-in or action ledger. Multi-effect
  durability is a process's job.
- The context, envelope, settlement, transcript, commit, remote and trace
  shapes change in place. In-flight pre-cutover invocations drain on their
  own generation.
