# 0100: The run-observation contract

## Status

Accepted 2026-09-21 (FIG-3475). This ADR decides the contracts for arc
FIG-3459 before its four coordinated cutovers. None of the target shapes below
is implemented by this ADR.

Amends [ADR 0037](0037-lashlang-workflows-use-a-code-graph-code-lens.md) and
[ADR 0073](0073-gradual-value-types-through-to-the-workflow-editor.md). The
dialect and VM work beyond decisions R7 and R8 belongs to FIG-3476.

The source investigation is
`/workspace/notes/lash/prospect-viz-2026-09-21.md`. Its replay-scope check is
`/workspace/notes/lash/prospect-viz-arc/verify-R0-replay-scope.md`.

## Context

Lash exposes a serializable workflow graph and an in-process trace-derived run
graph, but it does not expose one contract that lets a host pair a definition
with a running or completed process. Definition nodes and runtime sites use
different hashes. The runtime constructs a private pairing while building the
trace map. Node events reach only a host-installed `TraceSink`. The live
session replay buffer is bounded to 2,048 items and 120 seconds by default.
Durable process events have a sequence, but effect results have no bounded
host-readable process summary. These facts are visible in
`crates/lash-typescript/src/workflow_graph/mod.rs::GraphProjector`,
`crates/lashlang/src/tracking.rs::LashlangExecutionSiteBuilder::node_id`,
`crates/lash-lashlang-runtime/src/process.rs::append_trace_workflow_subgraph`,
`crates/lash-core/src/runtime/observation/replay.rs`, and
`crates/lash-core-execution/src/runtime/process/events.rs::ProcessEvent`.

The definition model and the runtime model also put different artifact
identities into their node-id preimages. The projector hashes `source_hash`
under `lash-workflow-node/v2`; the runtime hashes `module_ref` under
`lash-lashlang-execution-site/v2`. The relevant mint sites are
`crates/lash-typescript/src/workflow_graph/mod.rs::GraphProjector::node_id` and
`crates/lashlang/src/tracking.rs::LashlangExecutionSiteBuilder::node_id`.

The durable boundary is already clear elsewhere in Lash. The process event log
is durable semantic history, while a process event sink is best-effort
freshness. Live trace delivery does not become a generic durable trace bus.
See `crates/lash-core-execution/src/runtime/process/observation.rs`,
`docs/adr/0017-process-observation-is-best-effort-push-over-state-truth.md`,
and ADR 0099 section 13. The run-observation contract follows that boundary.

## Decision

Lash supplies host-facing data, identity, cursor, status, version, error, and
fold contracts. A host decides layout, rendering, interaction, drafts, and
mutation commands. `WorkflowGraph` remains the authoring document. A run view
is a projection over static map data, live observations, and the bounded
durable effect summary. No run-observation API prescribes a user interface.

Durable run state is a bounded semantic fold. Live node delivery is telemetry.
Lash does not capture a full per-node history by default, and it does not add a
trace store. Facts already recorded by effect result incorporation and process
transitions become readable through bounded projections. Missing live facts
remain unknown. This matches the process log and sink split in
`crates/lash-core-execution/src/runtime/process/observation.rs` and the live
replay gap model in `crates/lash-core/src/runtime/observation/replay.rs`.

### R0: one structural node identity

`lashlang` owns one `WorkflowNodeId` mint from structural owner and AST path.
The preimage contains no whole-artifact hash. Runtime sites are keyed under the
node as node id plus site kind. A node with several runtime sites keeps one node
id. The process-root graph node has no runtime site. Lifted process literals
keep their body digest in their owner and remain version-sensitive.

The lens and runtime must share one owner vocabulary and one kind vocabulary.
The current owner and kind inputs differ in
`crates/lash-typescript/src/workflow_graph/mod.rs` and
`crates/lashlang/src/tracking.rs`, so changing only the hash domain would not
make the ids equal. The single mint replaces both current v2 domains with one
guarded v3 domain.

Removing the artifact hash does not weaken effect replay identity. Effect keys
include the opener encoding, operation, node id, and occurrence in
`crates/lash-lashlang-runtime/src/host_identity.rs`. Process openers include
the process incarnation in
`crates/lash-core-store/src/effect_opener.rs::EffectOpener::identity_encoding`.
A process definition still pins its module artifact, and Lashlang child process
identity currently includes the module reference in
`crates/lash-core-execution/src/runtime/process/model.rs::ProcessIdentity` and
`crates/lash-lashlang-runtime/src/process_identity.rs`. SQLite and PostgreSQL
key effect replay rows by scope and replay key in
`crates/lash-sqlite-store/src/schema.rs` and
`crates/lash-postgres-store/schema.sql`. The Restate controller names an effect
from the same replay key in
`crates/lash-restate/src/controller/mod.rs`. The node-id change does move
`VmContinuation::occurrence_counters`, which is why the trace cutover must bump
and refuse the prior `LASHLANG_SEGMENT_STATE_VERSION`.

Version identity lives on the workflow document as `source_identity` and on
the trace execution identity as both `source_identity` and `module_ref`.
`source_identity` is the existing `lash-workflow-source/v3` digest minted in
`crates/lash-typescript/src/workflow_graph/mod.rs::GraphProjector::new`. It is
not the graph schema version, facet schema version, or executable module ref.

### R1: the map carries the site

Graph node ids and runtime node ids are equal by construction. A
`TraceLanguageExecutionMapNode` also carries its structured
`WorkflowExecutionSite`, including owner, AST path, and site kind. This lets a
host locate a node without first projecting the workflow. There is no pairing
table, secondary definition id, or join helper. Both map emitters, the process
emitter in `crates/lash-lashlang-runtime/src/process.rs` and the RLM cell
emitter in `crates/lash-protocol-rlm/src/executor`, use the same shape.

### R2: process-scoped subscription with epochs

Node observations ride a process-scoped subscription addressed by process id
and incarnation. Publication positions are contiguous only within one explicit
publisher epoch. The initial snapshot and its cursor describe one publication
boundary, so an observation racing snapshot acquisition is delivered exactly
once.

Ring overflow, expiry, subscriber lag, publisher replacement, unavailable
routing, process-id reuse, and cross-process loss return a typed gap and a
completeness-marked projection. They never imply that omitted nodes did not
run. The current live replay implementation already distinguishes retained
events from `Trimmed` and `Unavailable` gaps in
`crates/lash-core/src/runtime/observation/replay.rs`; the process cursor gets
its own encoding because a `SessionCursor` names session revision, not process
publication identity.

The session stream also gains every journaled process lifecycle kind,
including Abandoned, with the durable process-event sequence. Today
`SessionProcessEventKind` contains only Started and Cancelled in
`crates/lash-core/src/runtime/observation/replay.rs`, while terminal, wait, and
resume event names live in
`crates/lash-core-execution/src/runtime/process/events.rs`.

### R3: logical identity is not publication position

A node observation's logical identity is runtime node id, occurrence, admitted
attempt, and process incarnation. It is replay-stable across segments and is
the trace deduplication key. It is separate from the publisher epoch and
position. Lash promises no dense process-lifetime observation sequence.

The static execution map is independently obtainable. The fold is a pure,
deterministic, bounded function over a previous projection and observations.
It orders output canonically, treats an identical duplicate as a no-op, reports
a conflicting duplicate, never downgrades a terminal state for one occurrence,
and does not suppress a later occurrence. It carries explicit incomplete and
truncation markers. The current arrival-order mutation and unbounded seen-key
set are in `crates/lash-trace/src/lashlang_graph.rs` and are replaced by this
contract.

### R4: durable per-effect summary

Effect result incorporation persists a bounded process summary per effect
node. Each retained occurrence contains the runtime node id, occurrence,
operation, outcome class and code, and replay key. Each node has a configurable
occurrence cap and an `omitted` count. The summary contains no effect payload,
timing, or attempt. Pure computations, branches, and iterations do not gain
durable records.

Persistence runs through the recovery-aware result-incorporation path, not a
`TraceSink` callback. The append key is the effect's stable logical identity.
Re-incorporating a recorded result recovers the same event without executing
the effect again or appending a duplicate. A different payload under the same
key is a conflict. Normal process-event sequence allocation and transactional
projection remain in
`crates/lash-core-execution/src/runtime/process/validation.rs`. Process-event
pages use the exact process-and-incarnation read already exposed as
`ProcessRegistry::event_page_ref` in
`crates/lash-core-execution/src/runtime/process/registry_concerns.rs`.

### R5: attempt is telemetry identity only

Attempt and incarnation enter node-observation identity, trace graph keys, and
trace deduplication. They do not enter effect replay keys, effect group keys,
or Restate command addressing. The effect-key grammar in
`crates/lash-lashlang-runtime/src/host_identity.rs` stays byte-identical. This
keeps two telemetry attempts observable without realizing an external effect
twice.

### R6: expose existing source identity

`WorkflowGraph` exposes the projector's existing
`lash-workflow-source/v3` digest as `source_identity`. Projection from source
hashes canonical printed source. Projection from an IR that cannot be printed
uses the serialized-program fallback already selected by
`crates/lash-typescript/src/workflow_graph/mod.rs::workflow_graph_from_program`.
The trace identity receives the same value at the
`lash-lashlang-runtime` integration boundary.

Reconciliation only checks a submitted graph against its own canonical
reprojection. It reports pairs, unmatched nodes, and ambiguous nodes for real
insert or move cases. It does not semantically match nodes across arbitrary
revisions. R0 makes unrelated edits stop reminting every node id.

### R7: expression fields carry IR

Every expression-valued workflow graph field stores authoritative Lashlang IR.
A dialect parses host-edited text into that IR and prints the IR for display or
source output. Canonical dialect text is derived, not a second authoritative
field. Call and effect receivers and arguments use structured IR values, with
defined positional, named, nested-call, and record-field slot paths.

This replaces the current text representation in
`crates/lashlang/src/workflow_graph.rs::WorkflowNodeKind` and the TypeScript
reparse in
`crates/lash-typescript/src/workflow_graph/editable_text.rs::parse_expression_field`.
Type facets keep their derived, read-only role from ADR 0073. Diagnostics gain
an optional slot path and a closed Definite or Advisory classification.

### R8: the projector lives in the IR crate

The lens projector lives in `lashlang`, beside the IR. Dialects inject
expression print and parse operations. The engine crate no longer depends on
`lash-typescript` to build trace skeletons. The current dependency and call are
visible in `crates/lash-lashlang-runtime/Cargo.toml` and
`crates/lash-lashlang-runtime/src/process.rs::trace_lashlang_process_map`.
Neutral runtime modules, a dialect trait for execution, IR-expressed
intrinsics, truthiness, printer totality, and the wider dialect and VM split are
out of scope for this arc.

## Compatibility matrix

"Exact" means the decoder checks the named version before interpreting the
shape and refuses any other version. An unknown variant is always refused for
closed enums. "Tolerant fields" means a known variant or snapshot may ignore a
new field; it never means an unknown enum variant is accepted.

Every row has a checked-in JSON Schema document under `schemas/host/<shape>/`,
named `v<version>.schema.json` after its version owner and stamped with the
owner's constant in `x-lash-version-constant`. `python3
scripts/generate-workflow-schemas.py --check` (the `//:host_schema_check`
target) fails when a document drifts from its Rust owner or an obsolete
version is left beside the current one. The version owners below are the
constants on main when FIG-3469 closed the arc.

| Shape | Version owner | Decode rule | Unknown fields | Unknown variants | Schema document and enforcement |
| --- | --- | --- | --- | --- | --- |
| `WorkflowGraph` | `WORKFLOW_GRAPH_SCHEMA_VERSION = 15` in `crates/lashlang/src/workflow_graph.rs` | Exact, before document decode | Refuse. Same-version extra fields are unversioned shape drift. | Refuse. | `schemas/host/workflow-graph/`, with a closed root and a pinned `schema_version`. In `crates/lash-typescript/tests/workflow_graph.rs`: `nested_container_graphs_roundtrip_through_the_canonical_json_codec`, `workflow_graph_decode_checks_version_before_shape`, `workflow_graph_refuses_unknown_field`, and `workflow_graph_refuses_unknown_variant`; `crates/lash-typescript/tests/workflow_graph_schema.rs` validates real graphs against the published document. |
| Workflow type facets | `WORKFLOW_TYPE_FACET_SCHEMA_VERSION = 3` in `crates/lashlang/src/workflow_graph/facets.rs` | Exact when facets are read. A renderer may discard all facets without decoding them because they are non-authoritative. | Tolerate inside a known facet object so additive derived facts do not block graph rendering. | Refuse. | `schemas/host/workflow-type-facets/`. In `crates/lash-typescript/tests/workflow_graph.rs`: `type_facets_are_ignored_by_put_and_canonicalization`, `facet_reader_requires_exact_version`, `facet_reader_tolerates_unknown_field`, and `facet_reader_refuses_unknown_variant`. |
| `TypeExpr` | No standalone constant. The carrier owns it: graph fields use `WORKFLOW_GRAPH_SCHEMA_VERSION`; facet fields use `WORKFLOW_TYPE_FACET_SCHEMA_VERSION`. | Decode only inside a version-accepted carrier. | Refuse fields not defined by the selected variant. | Refuse. | Published inside both carrier documents above. `crates/lashlang/src/ast_tests.rs::process_signature_decode_refuses_missing_duplicate_unknown_and_legacy_fields`; in `crates/lash-typescript/tests/workflow_graph.rs`: `workflow_graph_refuses_unknown_type_expr_variant`, `workflow_graph_refuses_unknown_fields_inside_type_expr_payloads`, and `facet_reader_refuses_unknown_type_expr_variant`. `scripts/versioned-surfaces.toml` guards `TypeExpr` under both carriers. |
| `TraceRecord` and `TraceEvent` | `TRACE_SCHEMA_VERSION = 32` in `crates/lash-trace/src/lib.rs` | Exact record version before event decode. | Tolerate additive fields on a known record or event, as the existing trace policy documents. | Refuse and bump the trace version. | `schemas/host/trace-record/`, with an open root and a pinned `schema_version`. In `crates/lash-trace/tests/schema.rs`: `trace_schema_version_is_pinned_at_32`, `predecessor_trace_schema_is_refused_before_event_decode`, `unknown_attempt_usage_disposition_is_refused`, `known_trace_event_tolerates_unknown_field`, `published_trace_record_schema_accepts_every_event_and_payload_sample`, and `published_trace_record_schema_tolerates_additive_fields_and_refuses_unknown_variants`. |
| `TraceLashlangGraph` | `TRACE_SCHEMA_VERSION = 32`, carried as the snapshot's `schema_version`. | Exact snapshot version. | Tolerate additive fields on the trace-derived snapshot. | Refuse for status, observation, wait-kind, node-kind, and completeness enums. | `schemas/host/trace-lashlang-graph/`, with an open root and a pinned `schema_version`. In `crates/lash-trace/src/lashlang_graph/tests.rs`: `graph_decode_checks_version_before_shape_and_tolerates_additive_fields`, `graph_decode_refuses_unknown_closed_variant_at_the_current_version` (all five enum families), and `every_permutation_and_incremental_partition_is_byte_identical`; `crates/lash-trace/tests/schema.rs::published_graph_schema_accepts_a_folded_snapshot_and_enforces_its_row`. |
| Process subscription items | Local snapshot and event payloads use `TRACE_SCHEMA_VERSION = 32`; wire items use `REMOTE_PROTOCOL_VERSION = 93` in `crates/lash-remote-protocol/src/lib.rs`. Local cursors and gaps are typed live control values, not standalone stored documents. | Exact snapshot or remote-envelope version before payload decode. Cursor epoch mismatch yields a typed gap, not a compatibility decode. | Refuse on every new process-observation wire DTO with `deny_unknown_fields`. Trace payloads follow their tolerant-field rule above. | Refuse. | `schemas/host/remote-process-observation-request/` and `schemas/host/remote-process-observation-item/`; the item document types the snapshot graph as `TraceLashlangGraph` and the node event record as `TraceRecord`, each pinned to the trace version. In `crates/lash-remote-protocol/src/versioned_decode_tests.rs`: `observation_decode_checks_version_before_unknown_payload_tag`, `process_observation_snapshot_wire_contract`, `process_observation_event_wire_contract`, `process_observation_gap_wire_contract`, and `process_observation_cursor_wire_contract`. Each contract test covers an extra field and an unknown variant, and validates the real item against the published document. |
| Remote DTOs touched by the arc | `REMOTE_PROTOCOL_VERSION = 93` | Exact envelope negotiation. No compatibility decoder. | Refuse with `deny_unknown_fields`, including nested request and response DTOs. | Refuse. | `schemas/host/remote-process-events-request/`, `schemas/host/remote-process-events-response/`, and `schemas/host/remote-session-observation-event/`. `crates/lash-remote-protocol/src/tests.rs`: `remote_process_dtos_json_round_trip` and `immediate_predecessor_remote_protocol_generation_92_is_refused`; `versioned_decode_tests.rs::paged_process_events_wire_contract` and the four process-observation contract tests above. |
| Durable process events | `PROCESS_EVENT_VOCABULARY_VERSION = 1` in `crates/lash-core-execution/src/runtime/process/effect_summary.rs` | Exact vocabulary version for the bounded effect-summary kinds. No legacy decoder. | Refuse unknown fields in the effect-summary payloads. Existing custom producer event payloads remain producer-defined JSON. | Refuse unknown runtime-owned durable kinds; producer-declared custom kinds remain governed by their registered `ProcessEventType`. | `schemas/host/process-effect-outcome/` and `schemas/host/process-effect-omissions/`, each the payload schema the runtime registers for its kind. In `crates/lash-core-execution/src/runtime/process/validation_tests.rs`: `producer_cannot_override_runtime_lifecycle_event_types`, `process_event_vocabulary_version_is_pinned`, `effect_summary_refuses_predecessor_vocabulary`, `effect_summary_refuses_unknown_field`, and `effect_summary_refuses_unknown_runtime_kind`; both-backend fixtures in `crates/lash-core/tests/support/durable_read_fixture.rs`. |

The matrix is normative. An implementation that disagrees with a row is
brought to the row; the row is not weakened to match a decoder.

## Four cutovers

Each cutover is a clean replacement. A shape changes once, its constant moves
once, predecessor data is refused, and schema documents are generated in the
same integration. FIG-3469 supplies the generator beside each owner rather
than after all four cutovers.

The graph and trace cutovers share R0 and therefore cross one publication
boundary together. They may be reviewed as separate slices, but main may not
contain the new graph ids with the old runtime ids, or the new runtime ids with
the old graph ids. Their version constants still move once each.

Existing numeric constants advance by one from the value on main when their
cutover integrates. The values below record this decision's baseline; they do
not reserve a generation past a change that lands first.

Every cutover runs the shared floor on its committed, rebased head:

```text
kiln fmt -- --check
kiln clippy
kiln test
cargo nextest run --workspace --all-targets --locked --no-fail-fast --profile ci --ignore-default-filter
git diff --check
python3 scripts/check_version_bumps.py --base <merge-base>
```

The lists below add shape-specific fixtures and CI-only gates. They do not
replace the shared floor.

### Trace cutover

Tickets: FIG-3460, FIG-3461, FIG-3463, FIG-3474, and FIG-3465's trace identity
field.

This cutover moves the current `TRACE_SCHEMA_VERSION = 24` once. It also moves
the current `LASHLANG_SEGMENT_STATE_VERSION = 12` once because
`VmContinuation::occurrence_counters` is keyed by node id, changes
`lashlang-process-start/v2` to v3, and replaces
`lash-workflow-node/v2` plus `lash-lashlang-execution-site/v2` with the one v3
node domain. `WORKFLOW_GRAPH_SCHEMA_VERSION` moves with the graph cutover, not
a second time here; integration must not publish the R0 graph half against the
old graph version.

Fixtures and pins: `crates/lash-trace/tests/schema.rs`,
`crates/lash-lashlang-runtime/src/process/segment_trace_tests.rs`,
`crates/lashlang/src/runtime/tests/compiler_cases.rs`,
`crates/lash-protocol-rlm/src/executor/tests/lifecycle_and_diagnostics.rs`, and
the workflow graph literal-id and JSON goldens added by FIG-3465. Add a
predecessor fixture for a version-12 parked segment whose occurrence counters
use the old node ids. Update OTel projections in
`crates/lash-trace/src/otel.rs` and guard both mint sites plus `tracking.rs` in
`scripts/versioned-surfaces.toml`.

Additional gates: `kiln test //:feature_lane_tests //:feature_lane_clippy`,
the trace schema suite, the parked-segment refusal suite,
`bash scripts/check-workflow-graph-model.sh`, and a grep of
`scripts/confidence-gate.sh`, `justfile`, and `.github/` before any test rename.

### Graph cutover

Tickets: FIG-3465 and FIG-3467. R0's graph-side identity change from FIG-3460
integrates here so the document never exposes a half-cut-over id family.

This cutover moves the current `WORKFLOW_GRAPH_SCHEMA_VERSION = 10` once and
the current `WORKFLOW_TYPE_FACET_SCHEMA_VERSION = 2` once. It adds
`source_identity`, the
decode-first fence, guarded identity preimages, literal id and document
goldens, IR-backed expression fields, structured slots, and typed facet
diagnostics. It does not retain the text expression fields beside the IR.

Fixtures and pins: `crates/lash-typescript/tests/workflow_graph.rs`,
`examples/workflow-graph-roundtrip/tests/roundtrip.rs`,
`examples/workflow-graph-roundtrip/tests/type_facets.rs`, the new checked-in
schema documents, and `scripts/versioned-surfaces.toml` guards for `TypeExpr`,
the graph, facets, projector, and identity mint sites.

Additional gates: `bash scripts/check-workflow-graph-model.sh`, the graph and
facet goldens, the schema generator drift check, and
`kiln test //:feature_lane_tests //:feature_lane_clippy`.

### Remote cutover

Tickets: FIG-3473 and FIG-3462.

This cutover moves the current `REMOTE_PROTOCOL_VERSION = 83` once. It replaces
unbounded process-event reads with bounded full and lite pages, adds retention
outcomes, adds process observation snapshot, event, cursor, and gap DTOs, and
expands the session process lifecycle projection. Every new DTO uses
`deny_unknown_fields`; the envelope checks version before decoding its body.

Fixtures and pins: `crates/lash-remote-protocol/src/versioned_decode_tests.rs`,
`crates/lash-remote-protocol/src/tests.rs`, remote schema documents, rendered
SQLite and PostgreSQL query pins, and
`crates/lash-sim/tests/cross_backend_store_differential.rs`.

Additional gates: remote predecessor and schema tests, both-backend paging
tests, `scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh
pg-cross-backend`, and a confidence-shard selector grep before test renames.

### Durable cutover

Ticket: FIG-3464.

This cutover adds `PROCESS_EVENT_VOCABULARY_VERSION = 1`. It adds
the bounded effect-summary event kind through result incorporation. It does not
change `TRACE_SCHEMA_VERSION` or `REMOTE_PROTOCOL_VERSION`. The paged reader
from FIG-3473 is already present when this cutover lands.

Fixtures and pins: the runtime-owned vocabulary and validation tests in
`crates/lash-core-execution/src/runtime/process`, SQLite and PostgreSQL durable
read fixtures, Restate interruption captures, and the cross-backend store
differential oracle. Both stores must register the kind and preserve normal
event sequence allocation.

Additional gates: `crates/lash-sim/tests/schema_congruence.rs`, the SQLite and
PostgreSQL durable-read fixture suites,
`scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh pg-store`,
`scripts/ci/with-service.sh pg16 -- bash scripts/ci/store-tests.sh
pg-cross-backend`, and the Restate interruption and replay fixtures added by
FIG-3464.

## Consequences

A host can join definition and run data with one structural id, request the
static map independently, subscribe with bounded replay, detect incomplete
telemetry, fold observations deterministically, and reconstruct a bounded
effect outcome table after completion. None of those contracts chooses a
layout or stores a trace history.

The cutovers are intentionally incompatible. Old parked segments, graph
documents, trace records, remote messages, and durable effect-summary events
are refused by their owning version fence. There are no dual ids, fallback
decoders, compatibility aliases, or old expression-text fields.

Schema generation is part of each owning cutover. FIG-3469 closed the shared
generator, the drift gate, and the per-shape decode tests: every matrix row now
has its checked-in document under `schemas/host/`. The example frontend
consumes only the workflow graph and facet shapes, through types generated
from those documents; the trace, observation, and durable-event shapes reach
it only through the example's own run-event contract.
