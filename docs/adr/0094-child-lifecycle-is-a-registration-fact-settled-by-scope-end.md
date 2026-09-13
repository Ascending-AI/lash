# 0094: Child lifecycle is a registration fact settled by scope end

Status: Accepted

## Context

Process provenance and process lifecycle had been conflated in two incompatible
ways. [ADR 0011](0011-self-contained-processes.md) correctly made originator,
observer, wake, and session relations cleanup-free, but it left lifecycle as
unspecified host policy. [ADR 0042](0042-tool-attempts-are-atomic.md) then
attached an optional parent-end policy to recorded start intents and settled
those intents from turn-local or process-local action lists. That made
lifecycle depend on the start surface and on transient teardown state rather
than on the durable process registry.

The result could not state one rule for every child. Some registrations had no
policy, omitted tool-intent policy silently meant Cancel, and a child registered
after a parent-end plan could escape settlement. Cancellation was distinguished
by prose rather than a typed cause, and retries could overwrite or conflict on a
fresh timestamp. Start failure after registration also had no uniform recovery
rule.

This decision supersedes only ADR 0042's parent-end policy and settlement
design. It amends ADR 0011 with one explicit Lifecycle Relation; its other
provenance relations remain cleanup-free.

## Decision

A child process's Lifecycle Policy and Parent Scope are required facts on its
durable process record. They are written at registration and are never optional.
The parent's end is a durable registry fact. One registry sweep settles every
child whose policy says Cancel, for every parent kind. A child whose policy says
Abandon belongs to the host from registration onward.

Stop cancels only the process the turn is awaiting. A started handle outlives
the turn. Cancellation is cooperative: it is a request the child observes at
its next step or wake. Lashlang's `start P(x)` syntax and semantic hash do not
change, and the model neither sees nor writes a Lifecycle Policy.

### Who sets the policy

| Start surface | Policy writer | Required value |
| --- | --- | --- |
| Lashlang `start`, in both compilation dialects and bridges | Runtime bridge, on the model's behalf | Abandon; parent is the Turn or enclosing Runtime Process |
| Rust tool-provider start, including the surface used by `spawn_agent` | Tool author, through a required constructor argument | Author's choice; `spawn_agent` declares Abandon because it awaits inside the same call |
| Tool-intent start (`shell.start` is the only producer) | Tool author, in the request | Author's choice; absence is a typed decode refusal and shell declares Abandon |
| Host administration and session-manager start | Host, in the request | Author's choice; parent is Host unless the host names a Turn or Runtime Process |
| Remote-protocol start | Remote host | Author's choice; validation refuses absence |

The runtime owns the Cancel path because only it observes durable scope end and
survives a crash. The host owns the Abandon path. Lash keeps an abandoned child
manageable by recording its originator and Parent Scope, listing by parent,
showing pending cancellation with its Cancel Origin, and supporting cancel by id
or a host-selected session sweep. A row committed before start subsequently
fails is not host intent; the runtime compensates it.

### Target representations

#### Core types

`OnParentEnd` is `Abandon | Cancel`. `ParentScope` is one of:

- `Turn { session_id, turn_id }`;
- `Process { process_id, incarnation }`; or
- `Host`.

`ProcessLifecyclePolicy { parent, on_parent_end }` keeps the two required facts
together. It has no `Default` and no Serde default. Registration refuses
`Host + Cancel` because Host never ends. `ProcessStartRequest`,
`ProcessRegistration`, and `ProcessRecord` each carry exactly one required
`lifecycle` field. The process registration fingerprint includes that policy
and the resolved retry bound, advancing its identity family from 5 to 6.

`CancelOrigin` and `CancelRequest` live in `lash-sansio`, where
`ToolCancellation` can carry the typed origin. The origins are `TurnStopped`,
`ParentEnded`, `OperatorRequested`, `ModelRequested`, and `StartFailed`.
`CancelRequest` contains the origin, a requester rendering, and
`requested_at_ms`; it has no free-text reason. `ProcessRecord` adds an optional
boxed request, `ProcessTransition` adds `RequestCancel`, and cancellation
receipts report the origin. `ToolCancellation.origin` is additive and optional,
where absence means a legacy payload. `StartProcessIntent.on_parent_end` and the
defaulted `ProcessParentEndPolicy` disappear.

#### Event and replay identity

`process.cancel_requested` carries `CancelRequest` as typed JSON. Its v2 replay
key uses domain `lash.process-cancellation-request` and the preimage
`process_id · incarnation · origin · requester`. Two requests are the same only
when the same requester names the same origin for the same incarnation.

The event fold refuses a terminal row, records the first request, treats the
same origin and requester as a no-op without comparing the retry's timestamp,
and routes any different request through the typed transition-refusal path. A
`StartFailed` request for a never-started row without an external reference
folds directly to Cancelled. The projector dispatches typed event kinds; it has
no wildcard that can silently ignore a current event.

#### Storage and parent-end ledger

The PostgreSQL and SQLite `lash_processes` tables store and validate:

| Fact | Representation |
| --- | --- |
| Parent Scope | `parent_scope_kind` plus nullable `parent_scope_id`; Turn ids render as `session_id/turn_id`, Process ids as `process_id#incarnation`, and only Host has no id |
| Lifecycle action | Non-null `on_parent_end`, restricted to Abandon or Cancel; Host + Cancel is invalid |
| Pending cancellation | All-or-none `cancel_origin`, `cancel_requester`, and `cancel_requested_at_ms` |
| Lookup support | An index on Parent Scope and process id, plus a partial index over pending cancellation for every nonterminal status, including Caller Departed |

The old per-process action-list table is replaced by one
`lash_parent_end_plans` ledger keyed by `(parent_kind, parent_id)`, with
`ended_at_ms` and nullable `settled_at_ms`. A plan carries no action list. Its
actions are the index-served query for children with the matching Parent Scope
and Cancel policy, paged by process id.

On SQL tiers, writing the ledger row and selecting its children occur in one
registry transaction. Registration reads the ledger in its own transaction, so
a child commits before the end fact and is swept, or sees the fact afterward
and is refused. Historical rows with pending old plans become Process + Cancel;
all others become Host + Abandon, and `record_json` is rewritten to agree.
Stored registration fingerprints are not recomputed. Pruning preserves a
parent with an unsettled ledger row and reclaims settled ledger rows at the
ordinary horizon. The in-memory registry keys the ledger by kind and id;
both it and Restate retain the Lifecycle Policy on the process record, and
Restate journals the discriminated plan. `wake_session_id` deliberately remains
column-only.

#### Filters and observations

`ProcessListFilter` gains typed Parent Scope, pending-cancel-before, and typed
originator filters. `ProcessOriginatorFilter` distinguishes a Host (with an
optional Host Scope) from a Session; a Session match requires its Agent Frame
identity. These replace client-side provenance scans and the string-only
`originator_id` filter.

Observed process and work-item shapes stop storing fields derivable from
lifecycle and identity. Graph key, kind, status label, terminal flag, and label
become methods; Lifecycle Policy is exposed as its own fact. Remote observations
make the same cut and remove agreement validators for derived fields.

### Behaviour

#### Registration

Every start converges on `ProcessStartRequest`, whose required Lifecycle Policy
is validated before registration. Validation rejects Host + Cancel and a Turn
parent whose originator is not that session. A Cancel child whose Parent Scope
already has an end-ledger row is refused with typed `ParentEnded`; an identical
replay of an existing registration still returns its record.

#### Parent end

Each of the three turn exits and each terminal process completion writes one
parent-end ledger row. SQL tiers write it in the turn-commit or process-complete
transaction; Restate journals it immediately after commit. Host never ends, so
host shutdown uses Operator Requested cancellation over rows the host selects.

The process worker pages pending ledger rows, selects matching Cancel children,
requests cancellation with origin Parent Ended and the rendered Parent Scope as
requester, then marks the plan settled. Terminal children and children already
carrying a cancellation request count as settled. Concurrent sweeps converge;
one failed row retries on the next pass without aborting the page. Settlement
does not await child termination.

#### Stop and Cancel Origin

Stop is unchanged. Immediate stop requests cancellation only for the awaited
process, with origin Turn Stopped. After-step stop finishes the current step as
defined by
[ADR 0039](0039-turn-cancellation-is-a-first-party-work-driver-primitive.md).
Other children continue unless their Lifecycle Policy says Cancel, in which
case the parent-end sweep handles them.

The first Cancel Origin and requester win. The writer map is:

| Writer | Cancel Origin |
| --- | --- |
| Immediate await branch | Turn Stopped |
| Parent-end sweep | Parent Ended |
| Model `processes.cancel` | Model Requested |
| Process administration, session-runner control, and `cancel_all_visible` | Operator Requested |
| Start compensation | Start Failed |

A retry with the same origin and requester is a no-op. A different origin or
requester is a typed conflict, and the receipt reports the request that stands.
Turn-level cancellation remains separate.

#### Start compensation

No admitted-unconfirmed state or registration-plus-admission transaction is
introduced. On native tiers, the registered row is admission and the worker
poke is advisory: poke failure is logged and start returns the record.

On Restate, submission is keyed by segment: `process_id` for segment zero and
`process_id#ordinal` after handover. Submission coalesces. A submission error
requests cancellation with Start Failed inside the scheduling boundary before
returning the error; if that request also fails, start returns the record. A
recovery sweep re-reads and resubmits a nonterminal row only when it has neither
external reference nor cancel request. External references are compare-and-set,
including later segment ordinals, and terminal writes are journaled. The sweep
never terminalizes a row, and registration conflicts never cancel an existing
row.

#### Retry bound

Lashlang child registration resolves `max_attempts` from the runtime host
configuration instead of recording `None`. The bridge reads the default once at
segment start and carries it in segment state, so redrive after configuration
change uses the recorded value. The fingerprint includes the resolved bound;
rerunnable disposition remains unchanged. Retries are bounded by attempts, not
age, as established by
[ADR 0019](0019-process-recovery-obeys-declared-disposition.md).

### Cutover

Successor changes remove the displaced mechanisms when their replacements
land, without feature flags or compatibility shims:

- `RecordedToolIntentOutcomeBuffer` as settlement authority,
  `finish_parent_end_actions`, its three turn-exit call sites, and
  `ToolIntentParentEndAction` storage;
- `LashlangSegmentState.parent_end_actions` and restore handling;
- host-ingress `settle_parent_end`, `pending_tool_intent_parent_end`, and
  `complete_tool_intent_parent_end`, the turn-id-to-process-id cast, and the
  `parent_end_settled` submission flag;
- `lash_process_parent_end_plans`, its five registry methods, and both
  `complete_process_with_*_parent_end` variants;
- defaulted intent policy, `StartProcessIntent.on_parent_end`, and the decoder
  test that treats absence as Cancel;
- prose cancel reasons as discriminants,
  `cancellation_replay_key(process_id, reason)`, and the four event-counting
  derivations of pending cancellation in the execution context, process worker,
  and Restate workflow;
- the event projector wildcard, derived observation fields and agreement
  validators, and the in-memory originator scan;
- the false crash-reconstruction rustdoc; and
- the hard-coded Rerunnable construction in `prepare_lashlang_process_start`.

### Gates and versions

| Gate | At acceptance | Required change |
| --- | --- | --- |
| PostgreSQL component schema | 86 | Advance once from main at landing; add a migration and `introduced_relations` entries for the new ledger and indexes |
| SQLite process schema | 33 | Advance to 34, renumbering if main moved; do not move session or effect schema versions |
| Schema congruence | `process_parent_end_plans` pair | Rename the pair; keep process tables identical only when both SQL tiers add the same columns |
| Tier acceptance | No lifecycle suite | Prove the closure/registration race, fresh-timestamp cancel retry, failed compensation write, segment-one recovery key, and historical pending-plan migration on every tier |
| PostgreSQL schema witness | Literal shape listing | Update literal evidence for every added column and index |
| Durable-read fixtures | PostgreSQL dump plus SQLite fixture | Regenerate both with surrogate-escape handling and prove no unrelated signature moved |
| Tool-output and remote formats | Pinned constants | Advance for optional cancellation origin and required start lifecycle fields |
| Registration identity | Family 5 | Advance to family 6 for Lifecycle Policy and resolved retry bound |
| Lashlang identity | Semantic hash v8 | Keep unchanged |

## Consequences

- Lifecycle is inspectable and enforceable from one durable registry record,
  independent of which start surface created the process.
- Parent end is race-free with registration and redrivable without transient
  action lists.
- Cancellation reports a stable typed cause and requester; retries cannot
  rewrite that fact.
- Abandon remains an explicit host obligation rather than hidden cleanup.
- Pre-1.0 stored registrations and schemas cut over to the new required shape;
  there are no optional fields or legacy decode defaults.

## Non-goals

This decision adds no hard-kill primitive, automatic cancellation of unawaited
children, Lashlang syntax change, nursery scoping, per-parent admission cap, or
registration-plus-admission transaction. It does not change checkpoint
backtracking, after-step Stop, cancellation observation at a step or wake, or
recovery's obligation to follow the declared disposition.
