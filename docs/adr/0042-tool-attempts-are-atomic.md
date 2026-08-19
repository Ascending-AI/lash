# Tool attempts are atomic

Tool implementations are opaque host code. Lash cannot reliably discover,
name, order, or replay every network call, database write, timer, or other side
effect performed while a tool runs. Pretending that those operations compose
as nested durable effects creates a leaky boundary whose behavior differs by
execution tier.

The contract is therefore:

> Attempts are atomic. In-attempt effects are opaque. Durable composition lives
> in the process layer.

One prepared tool attempt is one journaled `ToolAttempt` entry. Everything the
tool performs before returning its result belongs to that entry, including a
direct completion and a retry delay. A direct completion still uses the normal
session-manager request plan and the normal post-outcome usage, trace, and
bookkeeping path, but it executes locally while the parent attempt is open. It
does not submit a nested `Direct` envelope. This decision depends only on the
operation's position inside a tool attempt, so inline and workflow-backed tiers
follow the same path.

The implementation law has two provider shapes:

> If you need to await durable work mid-body, you are orchestration — declare
> process shape. If you only need to cause it, return an intent.

Leaf providers receive `AttemptContext` and execute as one opaque recorded
attempt. Orchestrating tools are registered through a distinct typed lane as
completed `OrchestratingToolDef` values whose concrete constructors and
implementation types are owned by `lash-protocol-standard` and
`lash-subagents`; the core wrapper has private fields and no public safe
constructor. Its doc-hidden unsafe capability constructor requires the caller
to own the tool contract, so safe out-of-tree code cannot mint one. The
boundary is an auditable unsafe capability convention, not construction-level
enforcement or a memory-safety invariant: a deliberate out-of-tree host can
enter the lane with an `unsafe` act that violates provenance without that act
itself causing undefined behavior, and `#![deny(unsafe_code)]` keeps the act
visible. The registry never recognizes a tool id or plugin id and upgrades its
provider: a leaf registration remains a leaf for every identifier.
The law `leaf_and_orchestrating_tool_id_collision_is_typed` pins the advertised
case: when both lanes advertise the same id, reconciliation fails with
`CrossLaneToolIdCollision`. Dispatch matches the bound registration kind and
resolves through a typed source key whose leaf and orchestrating variants are
disjoint, so even a leaf plugin id that renders like an internal source cannot
select or replace the orchestrating route. Dispatch then
hands `OrchestrationContext` directly to the orchestrating implementation; no
marker is smuggled through `ToolContext`, and no external leaf provider can opt
in by naming itself or its tool specially. The first-party bodies never enter
`coordinate_tool_invocation` and have no enclosing `ToolAttempt`. The body is authored process-replay code,
so every journal command it issues is a direct child of the enclosing process
invocation. `ExecCode`, runtime-owned `batch`, and `spawn_agent` use that replay
shape; leaf tools cannot recursively dispatch a batch.

Persisted registration kind is only a routing hint until a live source binds
the id. On restore and subsequent reconciliation, the live source is
authoritative and re-derives the lane; an unresolved orphan remains
non-executable until that source reappears. This is what lets pre-cutover
snapshots restore even though they carry no lane field. The law
`snapshot_resolution_rejects_lazy_live_sources_from_both_lanes` pins the other
typed-collision case: a snapshot-only id lazily resolved by both lanes fails
with `CrossLaneToolIdCollision`. The law
`lazy_leaf_resolution_cannot_smuggle_an_orchestrating_registration` pins the
mixed advertised/lazy case: the advertised lane's source-derived kind binds
silently and the lazy claim cannot replace it, so dispatch remains fail-closed.

The orchestration-body determinism contract is binding: no wall clock, random
number generation, or unordered iteration may drive commands; no unjournaled
I/O is permitted; and every journaled action is immediately awaited. The
`orchestrating-tool-determinism` pre-commit lint is defense in depth: it
lexically inspects each `execute_orchestration` and
`execute_orchestration_by_id` body, resolves direct import aliases, and rejects
cheap-to-detect inline violations. It deliberately does not claim transitive
helper analysis. Review owns the authored call graph, while crash-redrive and
structural runtime laws enforce the binding contract at execution: `batch` and
`spawn_agent` have no `ToolAttempt` frame of their own, and their journaled
children retain stable direct lineage.

An internal owner-bound `ProcessInput::ToolCall` body is a different boundary:
it is the process activity itself and may perform host I/O. Core executes an
`Internal` process tool directly, with panic containment but without a
`ToolAttempt`; that route is not exposed to model-facing providers as an escape
from the orchestration determinism contract. The internal shell runner uses
this boundary for PTY ownership and registry point-read/backoff signal-event
waits; those waits do not consume journal ordinals.

The structural rule remains:

> A recorded body must not emit commands into an ordinal-addressed journal.

FIG-1294 enforced the leaf boundary by construction for providers that opted in
through `supports_attempt_context`. FIG-1487 removed the opt-in: every recorded
attempt body now receives `AttemptContext`, whatever the provider. Its
process/session projections are controller-free reads, and the trybuild fixture
`attempt_context_has_no_journal_capability.rs` proves that the leaf type cannot
obtain a controller-backed process scope. The law
`sentinel_allows_no_undeclared_crossing_from_inside_an_attempt` proves exactly
zero controller crossings while a leaf attempt body is open, and
`sentinel_test_only_leak_trips_inside_a_recorded_attempt` proves the detector
still trips. `ToolContext` no longer exposes process administration, while
owner-bound internal process bodies receive `InternalProcessContext` and
authored orchestration receives `OrchestrationContext`.

One capability the leaf type does not have needed a declarative replacement: a
deferring tool that must announce its durable wait — the await key an external
resolver delivers against — cannot append the process event itself. It declares
the event on its `PendingCompletion` through `PendingAnnouncement`, and the
runtime appends it at park time, after the completion key is taken and before
the call is handed back as pending. The announcement therefore exists if and
only if the park happened, a failed append fails the call instead of parking
silently, and the required replay key makes the append idempotent across
redrives of the attempt. This is a single runtime-executed carve-out on the
pending return, not a body-side door: `AttemptContext` still has no
`process_events()`, and `ToolAttemptResult::Pending` still cannot carry general
intents.

Because no attempt body can hold a `ToolContext`, the three runtime refusals
that FIG-1294 kept for un-opted providers (`sessions().start_turn()`,
`dispatch().batch()`, and `triggers().emit()` refusing on ordinal-addressed
tiers) are unreachable and are deleted. The guarantee is now stronger and
type-level rather than tier-conditional: the journal-capable methods are simply
absent from the type a recorded attempt receives.

Layer 2 reclassified the shipped tool routes before that deletion:
`shell.start`/detach declare `StartProcess`, `shell.write` declares
`SignalProcess`, and `processes.cancel` declares `CancelProcess`; `spawn_agent`
and protocol-standard `batch` use process-replay orchestration. The five tools
therefore have one behavior on every tier without a compatibility guard.

FIG-1291 ships the capability-separated authoring model, and FIG-1487 makes it
unconditional: every recorded attempt body receives the sealed, controller-free
`AttemptContext`. A completed attempt returns `ToolAttemptResult::Done` with a
`ToolResultDone` and versioned `ToolIntents`; a deferred attempt returns
`ToolAttemptResult::Pending`, whose type cannot carry intents. Lash records the
final attempt first, then admits and drains its declarations in source order.
Retries discard non-final declarations. Each v1 declaration derives one stable
identity from `(session_id, execution_scope_id, tool_call_id, intent_index)`;
the execution-scope component is the turn id in turn scope and the process id
in process scope. FIG-1203 remains the rebase point for frame-key-grade call
identity.

Realization is journal-first. The recorded intent issues exactly one process
command with its identity-derived replay key. It does not re-read visibility,
existence, terminal state, the live tool filter, or live host configuration
before that durable boundary. Deterministic admission uses only recorded data.
Unknown/terminal/conflict results are recorded command outcomes, so redrive
replays identical evidence even if the live registry changes. A start's env
spec, observers, input, and parent-end policy all come from the recorded
payload. Tool visibility is therefore an authoring-time catalog decision, not
a replay-time intent gate; a future contract requiring such a gate must record
it as a separate admission fact. The laws
`start_env_is_persisted_after_admission_and_matching_redrive_completes` and
`start_env_store_error_is_typed_and_registers_no_process` pin both sides of the
journal-first boundary: environment state is written only after admission, and
a failed environment write cannot leave a registered process behind.

After the enclosing turn or process reaches its end, recorded start intents are
handled by a deterministic parent-end step. Version 1 deliberately exposes only
`Abandon` and `Cancel`, with `Cancel` as the default: `Abandon` is a recorded
no-op, while both policies are adjudicated by one replay-keyed
`ProcessCommand::ParentEnd` carrying the complete validated intent identity and
returning a typed `Abandoned`, `Cancelled`, or `Refused` outcome. Lash processes
are cooperative and Lash has no
hard-kill primitive; engines own their own kill semantics. Temporal's three-way
Parent Close Policy therefore maps to Lash as `{Abandon, Cancel}`. This is a
recorded model deviation, not an omitted implementation. If a hard-kill
primitive earns its way into the process model, reject-and-recreate versioning
adds the policy only together with that primitive, never ahead of it.

Outside a turn, a host binds `LashCore::tool_intents` to an actual session and
`ExecutionScope`, derives a `ToolIntentIngressKey` from the same
`(session_id, execution_scope_id, tool_call_id, intent_index)` identity, and
submits one typed intent through `ToolIntentIngress::submit`. This is the sole
host front door for durable leaf-style declarations. The identity-derived
replay key is validated before admission and drives the process effect, so a
controller-owned key-addressed journal returns the first typed outcome for an
identical duplicate and marks it `replayed`; the laws
`duplicate_host_submit_returns_the_same_outcome_and_realizes_once` and
`host_ingress_duplicate_replays_the_same_outcome_once_on_postgres` prove the
in-memory and real PostgreSQL front doors. A reused identity whose replayed
outcome has a different intent kind returns
`IdentityBoundToDifferentIntent`, as proved by
`identity_reused_from_start_to_emit_is_a_typed_refusal_without_panicking` and
`identity_reused_from_emit_to_cancel_cannot_fabricate_cancel_success`.
Runtime-owned tiers bind the first submitted envelope in the process registry,
so all ingress handles and all process targets observe one authoritative
identity record. The laws
`runtime_owned_identity_is_bound_before_a_different_target_is_submitted` and
`runtime_owned_identity_gate_is_shared_across_independent_ingress_handles` prove
that scope. A process-store replay-key collision surfaces as the typed
`DuplicateIdentity` ingress refusal, proved by
`runtime_owned_duplicate_identity_is_a_typed_ingress_refusal` and, for
identical, changed-reason, and concurrent cancellation duplicates,
`runtime_owned_cancel_duplicate_identity_is_typed_and_realizes_once`. On an
ordinal-addressed tier every external submit is a new engine invocation rather
than a key lookup, so a host must not treat a second invocation as an ingress
idempotency retry; `checked_in_tool_intent_journals_replay_through_endpoint_with_literal_outcomes`
pins replay only within the owning Restate invocation. The law
`crash_after_admission_redrives_to_exactly_one_realization` durably records the
mock admission's canonical envelope hash before its injected crash, rejects a
changed-payload redrive, and realizes the originally admitted command once, while
`attempt_with_nested_command_redrives_identically_on_the_key_addressed_tier` and
`recorded_intent_command_replays_after_live_terminal_mutation_on_postgres` pin
the real journal-first window. Foreign session/scope keys and malformed
transport keys return typed ingress refusals, as proved by
`foreign_session_and_turn_keys_are_typed_refusals` and
`malformed_key_is_a_typed_refusal_before_realization`.

For a process parent, "reaches its end" means after its terminal outcome is
durable. The terminal write atomically retains a compact
`ToolIntentParentEndAction` plan; teardown runs afterward and clears the plan
only after every replay-keyed `ParentEnd` command settles. A crash after the
terminal write, including between two commands, therefore redrives the plan.
Lashlang segment state version 2 carries the compact actions across segment
boundaries so an early-segment start is governed at the real process end.
Successful and refused adjudications emit structured evidence containing the
full identity, policy, child process id, and typed outcome. The public ingress
facade reconstructs and settles the retained plan after a host scope is rebound;
`ingress_start_default_cancel_is_retained_and_settled_after_scope_rebind` proves
that the default `Cancel` policy survives that boundary and is redrive-safe.

> **Historical versions.** The version numbers in this ADR record the state at ratification. The current values live in `lash::formats`; see `scripts/check_format_versions.py`.

`shell.start` and its detached form map explicitly to `Abandon`: their
owner-bound commands intentionally continue across turns, so the generic
default `Cancel` would contradict the tool lifecycle. Their leaf result prints
the identity-derived process id; the coordinator drains the recorded start
before any later model step. `shell.write` and `processes.cancel` project their
recorded signal/cancel requests and receive typed intent outcome addenda.

Protocol-standard retains the model-facing `batch` contract and argument/result
projection, while its execution is runtime-owned orchestration through
`OrchestrationContext::call_tool_batch`. This is the minimal home: protocol owns
syntax, limits, and projection; runtime owns recursive scheduling and durable
command frames. `spawn_agent` likewise starts its prepared child and
immediately awaits that exact process through the orchestration context.

The former `ToolContext::durable_effects()` facade and its `DurableStep`
producer are removed, including the serialized command and outcome. External
waits use deferred tool completion when the whole job has one eventual tool
result. Workflows with multiple durable effects, waits, or decisions model each
boundary as a process step and pass durable data between those steps.

Atomic attempts provide replay after a completed outcome: replay returns the
recorded tool result without invoking the provider or any other in-attempt work
again. They cannot provide exactly-once execution across an unrecorded
completion. If an attempt is retried, or the worker crashes after an
in-attempt effect succeeds but before the attempt outcome is recorded, the
whole attempt runs again. In-attempt effects are consequently at-least-once;
an LLM call can be billed again. Tool authors must make external writes
idempotent when needed and move independently durable boundaries into process
steps.
