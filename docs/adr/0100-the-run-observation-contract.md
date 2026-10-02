# 0100: The run-observation contract

## Status

Accepted.

## Context

A host needs to join an executable definition with its running or completed
process while distinguishing durable semantic facts from missing telemetry.
A bounded live stream cannot prove that an omitted node never ran.

## Decision

Lash supplies identities, static maps, cursors, gaps, status projections,
bounded effect summaries, and versioned host data. Hosts choose layout,
rendering, interaction, and mutation commands. `WorkflowGraph` is the authoring
document; a run view combines its static map with observations and durable
semantic history. Live trace publication is best-effort and cannot fail a
process commit. It is not a durable trace history.

### R0: one structural node identity

`lashlang` mints `WorkflowNodeId` from structural owner and AST path under
`lash-workflow-node/v3`. The preimage contains no whole-artifact hash.
Runtime sites add their site kind under that node. Process-root graph nodes
have no runtime site. A lifted literal's owner contains its body digest.

Version identity stays on the document as `source_identity`, and trace identity
also carries `module_ref`. Effect addressing has a separate owner. VM commands
are named positionally by issue ordinal through `CodeCallIdentities`; node id,
occurrence, and telemetry attempt do not name external effects. A process
opener contains the minted process id. Editing a structural node cannot weaken
the journal's command addressing contract.

Evidence: `crates/lashlang/src/workflow_graph.rs::workflow_node_id`,
`crates/lashlang/src/tracking.rs:50`,
`crates/lashlang/src/workflow_graph/projection.rs:203`, `:307`, and
`crates/lash-lashlang-runtime/src/host_identity.rs:1`.

### R1: the map carries the site

Graph and runtime node ids come from the same structural mint.
`TraceLanguageExecutionMapNode` carries `WorkflowExecutionSite`, including
owner, AST path, and site kind. A host needs no pairing table or secondary id.

`ModuleArtifact::ir` is the linked, span-free executable program, with names
verbatim. Private artifact fields admit it through linking, the validating
builder, or the verifying store decoder. The compiler and projector read that
program. Structural roles, binding visibility, loop bind expressions, and
`ProcessOrigin::Lifted` describe generated structure without readers guessing
front-end names. The process and RLM emitters use the same map contract.

Evidence: `crates/lashlang/src/artifact.rs`,
`crates/lashlang/src/workflow_graph/projection.rs:51`,
`crates/lashlang/src/workflow_graph/execution_sites.rs:1`,
`crates/lash-lashlang-runtime/src/process/trace_map.rs`, and
`crates/lash-protocol-rlm/src/executor`.

### R2: process-scoped subscription with epochs

A subscription names one minted process id. Publication positions are
contiguous only within a publisher epoch. The process cursor is
`lashpc3:<epoch>:<process-reference>:<position>:<sequence>`.
The reference is the minted process id; position is the live publication
position and sequence is the durable event high-water mark.

`Processes::events` pages durable history with Full or Lite payload selection.
A subscription resumes without a gap only when the ring retains every item
past the cursor's position and `Committed { sequence }` evidence bridges its
sequence to the durable high-water mark. Epoch replacement, trim, lag,
unavailable routing, a different process, unavailable history, or an unbridged
sequence returns a typed gap, snapshot, and new cursor, including for idle work.

The snapshot combines a live graph at its publication boundary with status and
an effect-summary fold through one durable high-water mark. Acquisition is
bounded and uses Full payloads with typed retention outcomes. Live completeness
and durable completeness are separate. Missing observations stay unknown.
Durable publication follows the commit through the process sink.

Evidence: `crates/lash-sansio/src/process_cursor.rs:1`, `:55`,
`crates/lash/src/process_observation.rs:1`, `:48`, `:591`, `:725`, and
`crates/lash-core-execution/src/runtime/process/observation.rs`.

### R3: logical identity is not publication position

Node observations use node id, node kind, occurrence, and attempt within their
execution identity. That identity is distinct from publisher epoch and position.
The static map is independently available. The bounded pure fold keeps
canonical output across arrival permutations and incremental partitions,
deduplicates equal observations, reports conflicting duplicates, preserves
terminal state per occurrence, and admits later occurrences. It exposes
incompleteness and truncation rather than implying absent work did not execute.

Evidence: `crates/lash-trace/src/lashlang_graph/model.rs:24`,
`crates/lash-trace/src/lashlang_graph.rs:55`, `:183`, `:1420`, and
`crates/lash-trace/src/lashlang_graph/tests.rs`.

### R4: durable per-effect summary

Result incorporation records the first `PROCESS_EFFECT_OCCURRENCE_CAP`, 8,
occurrences per effect node. Each contains node id, occurrence, operation,
outcome class, optional code, and replay key, with no payload, timing, or
attempt. Later occurrences contribute to bounded omission counts. Replay-stable
progress and pending entries travel in segment state.

Pending occurrences commit at the next process boundary: entering or clearing
a wait, a body event, or terminal completion. They are the prelude of that
boundary's atomic event batch; terminal batches add omissions before the
terminal event. A segment boundary carries state and commits no summary itself.
The fold updates the process projection and change clock as one committed batch.

Repeating an equal replay key is a no-op. A different payload under it refuses
the batch. Failed summary incorporation is infrastructure failure for redrive,
not an error delivered to the program. Restate rebuilds through journal replay
and segment state. This does not promise exactly-once external I/O before the
journal settles. Pure computations, branches, and iterations gain no durable
summary events.

Evidence: `crates/lash-core-execution/src/runtime/process/effect_summary.rs:26`,
`:62`, `:79`, `:153`,
`crates/lash-core-execution/src/runtime/process/validation.rs`, and
`crates/lash-conformance/src/conformance/process_event_batch.rs:1`.

### R5: attempt is telemetry identity only

Attempt distinguishes telemetry observations and trace deduplication. It does
not enter effect replay keys, group keys, or Restate command addresses. Process
identity is the minted id, with no additional process-incarnation component.

Evidence: `crates/lash-trace/src/lashlang_graph/model.rs:24`,
`crates/lash-lashlang-runtime/src/host_identity.rs:1`, and
`crates/lash-sansio/src/process_cursor.rs:55`.

### R6: expose the admitted artifact's identity

`WorkflowGraph::source_identity` is `ModuleArtifact::source_identity`, the
`lash-workflow-source/v4` digest of the linked span-free IR atom stream.
It is not a text digest or serializer spelling. A draft projected from source
has no runtime identity. An admitted artifact supplies the runnable projection
and trace identity. Reconciliation checks a graph against its own canonical
reprojection and reports matches, unmatched nodes, and ambiguity; it does not
match arbitrary revisions semantically.

Evidence: `crates/lashlang/src/artifact.rs:281`,
`crates/lashlang/src/workflow_graph/projection.rs:51`, `:79`, and
`crates/lashlang/src/workflow_graph.rs:455`.

### R7: expression fields carry IR

Expression-valued graph fields store authoritative IR. A dialect parses edited
text into IR and prints IR for display or output. Canonical text is derived.
Call receivers and arguments use structured expressions and typed positional,
named, nested-call, field, and index slot paths. Type facets are derived,
read-only facts. Diagnostics carry an optional slot path and a closed Definite
or Advisory classification. Definite reports an admission failure for the
analyzed program and host environment. Advisory gives advice without
establishing an admission failure. Every current linker diagnostic kind is
Definite. The facet reader requires the classification and refuses unknown
classification variants.

Evidence: `crates/lashlang/src/workflow_graph.rs:665`, `:735`, and
`crates/lashlang/src/workflow_graph/facets.rs:96`, `:154`.

### R8: the projector lives in the IR crate

`lashlang::WorkflowGraphProjector` owns projection beside IR. A dialect supplies
statement text through `WorkflowStatementText`; trace maps use `NoStatementText`.
The process engine does not depend on `lash-typescript` for trace skeletons.
`check-workflow-graph-model.sh` guards that dependency boundary.

Evidence: `crates/lashlang/src/workflow_graph/projection.rs:1`,
`crates/lash-typescript/src/workflow_graph/mod.rs:478`,
`crates/lash-lashlang-runtime/Cargo.toml`, and
`scripts/check-workflow-graph-model.sh`.

## Compatibility matrix

Compatibility is specific to each carrier. Unknown closed variants are refused.
Tolerant fields do not imply tolerant variants. The current constants below name
the ordinary build; synthetic-next declares its supported next tier for upgrade
proofs. Fleet-selected read windows and upcasters follow
[ADR 0115](0115-the-1-0-binary-carries-its-half-of-every-upgrade.md).
The pre-1.0 freeze governs changes in place.

| Shape | Version owner | Version admission | Fields and variants |
| --- | --- | --- | --- |
| `WorkflowGraph` | `WORKFLOW_GRAPH_SCHEMA_VERSION = 21` | Version checked before shape against the fleet read window; derived projections regenerate rather than upcast | Unknown authoritative fields and closed variants refused |
| Type facets | `WORKFLOW_TYPE_FACET_SCHEMA_VERSION = 4` | Noncurrent facets discarded before graph decode; consumers may discard derived facets | Additive known-facet fields tolerated; closed variants refused |
| `TypeExpr` | Its graph or facet carrier | Decoded only in an admitted carrier | Unknown variant fields and variants refused |
| Trace records and events | `TRACE_SCHEMA_VERSION = 36` | Exact trace record version before event decode | Additive known fields tolerated; closed variants refused |
| `TraceLashlangGraph` | `TRACE_SCHEMA_VERSION = 36` | Exact snapshot version before shape | Additive fields tolerated; closed status, observation, wait, node, and completeness variants refused |
| Process observation wire items | `REMOTE_PROTOCOL_VERSION = 100`, trace version for nested payloads | Remote envelope negotiation; epoch mismatch is a live gap | Wire DTOs refuse unknown fields and variants; nested trace follows trace policy |
| Process event wire pages | `REMOTE_PROTOCOL_VERSION = 100` | Remote envelope negotiation | DTO fields and variants closed; Full/Lite is request data |
| Durable effect-summary events | `PROCESS_EVENT_VOCABULARY_VERSION = 1` | Fleet read window and registered upcaster before strict payload decode | Unknown summary fields and runtime-owned kinds refused; declared custom payloads remain producer-owned |

The owners and fences are in `crates/lashlang/src/workflow_graph.rs:61`, `:129`,
`crates/lashlang/src/workflow_graph/facets.rs:12`,
`crates/lash-trace/src/lib.rs:164`, `:187`,
`crates/lash-trace/src/lashlang_graph/model.rs:140`,
`crates/lash-remote-protocol/src/lib.rs:315`, and
`crates/lash-core-execution/src/runtime/process/effect_summary.rs:17`, `:125`.

Checked-in JSON Schemas live under `schemas/host/`. Their generator and drift
check are `scripts/generate-workflow-schemas.py`. Full event SQL selects payloads;
Lite SQL selects sequence and event type with a bound and never fetches the
payload. The query pin is
`crates/lash-store-sql/src/process/events.rs:28`, `:33`, `:64`.

## Alternatives considered

A durable per-node trace bus duplicates process semantic history and retains
unbounded telemetry. The bounded effect summary covers durable outcomes while
live gaps remain explicit. Artifact hashes in structural node ids remint
unrelated nodes after edits; executable identity belongs on the carrier.
Blanket additive decoding weakens authoritative graph and remote shapes;
compatibility follows each carrier's actual role.

## Consequences

Hosts join maps and run data by structural id, subscribe with gap detection,
fold bounded telemetry, and read durable effect outcomes without storing a
trace history. Carrier owners enforce decode policy and published schemas.
Evidence includes graph carrier laws, trace fold/schema tests, process
observation tests, event-page SQL pins, and event-batch conformance.
Store laws use SQLite file, SQLite memory, and PostgreSQL. Host laws use the
in-process Restate server double, live Restate, and lash-sim's in-process effect
host. Upgrade proofs use synthetic-next.

## Model usage accounting

The pre-journal limit is an explicit liability, not silent loss. A provider
attempt is dispatched only under an admitted usage meter, so a charge the
journal cannot describe (a body re-run after an unrecorded fault, an execution
killed between its entry and its send, facts too large to journal beside a
poison entry) is an `unknown` run with a reason, readable through
`LashCore::owner_usage` ([ADR 0125](0125-model-usage-is-engine-owned-accounting-delivered-per-call.md)).
