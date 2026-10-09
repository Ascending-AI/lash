# 0100: The run-observation contract

## Status

Accepted.

## Context

A host needs to join an executable definition with its running or completed
process while distinguishing durable semantic facts from missing telemetry.
A bounded live stream cannot prove that an omitted node never ran.

## Decision

Lash supplies identities, static maps, cursors, gaps, status projections,
bounded effect summaries, and local Rust values. Hosts choose layout,
rendering, interaction, and mutation commands. `WorkflowGraph` is the authoring
document; a run view combines its static map with observations and durable
semantic history. Live trace publication is best-effort and cannot fail a
process commit. It is not a durable trace history.

### R0: one structural node identity

`lash_vm` mints `WorkflowNodeId` from structural owner and AST path under
`lash-workflow-node/v3`. The preimage contains no whole-artifact hash.
Runtime sites add their site kind under that node. Process-root graph nodes
have no runtime site. A lifted literal's owner contains its body digest.

Version identity stays on the document as `source_identity`, and trace identity
also carries `module_ref`. Effect addressing has a separate owner. VM operations
are named by their admitted issue ordinal through `CodeCallIdentities`, which
commits with the VM snapshot
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §8); node
id, occurrence, and telemetry attempt do not name external effects. A process
opener contains the minted process id. Editing a structural node cannot weaken
that admitted identity.

Evidence: `crates/lash-vm/src/workflow_graph.rs::workflow_node_id`,
`crates/lash-vm/src/tracking.rs:50`,
`crates/lash-vm/src/workflow_graph/projection.rs:203`, `:307`, and
`crates/lash-vm-runtime/src/host_identity.rs:1`.

### R1: the document carries the site

Graph and runtime node ids come from the same structural mint. Each document
node states its `WorkflowExecutionSite`s, including owner, AST path, and site
kind. An execution's start names that document by reference
(`WorkflowDocumentRef`) and carries no copy of it; events name exact sites
and a host looks labels and kinds up in the document. A host needs no pairing
table or secondary id.

`ModuleArtifact::ir` is the linked, span-free executable program, with names
verbatim. Private artifact fields admit it through linking, the validating
builder, or the verifying store decoder. The compiler and projector read that
program. Structural roles, binding visibility, loop bind expressions, and
`ProcessOrigin::Lifted` describe generated structure without readers guessing
front-end names. The process and RLM emitters name the same document contract.

Evidence: `crates/lash-vm/src/artifact.rs`,
`crates/lash-vm/src/workflow_graph/projection.rs:51`,
`crates/lash-vm/src/workflow_graph/execution_sites.rs:1`,
`crates/lash-vm-runtime/src/document.rs`, and
`crates/lash-protocol-rlm/src/executor`.

### R2: process-scoped subscription with epochs

A subscription names one minted process id. Publication positions are
contiguous only within a publisher epoch. The process cursor is
`lashpo1:<incarnation>:<sequence>:<position>:<process-id>`.
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

Evidence: `crates/lash-core/src/runtime/observation/process_replay.rs`,
`crates/lash/src/process_feed.rs`, and
`crates/lash-core-execution/src/runtime/process/observation.rs`.

### R3: logical identity is not publication position

Node observations use the exact site, its occurrence, and attempt within their
execution identity. That identity is distinct from publisher epoch and position.
The static document is independently available by its reference. The bounded pure fold keeps
canonical output across arrival permutations and incremental partitions,
deduplicates equal observations, reports conflicting duplicates, preserves
terminal state per occurrence, and admits later occurrences. It exposes
incompleteness and truncation rather than implying absent work did not execute.

Evidence: `crates/lash-trace/src/workflow_overlay/model.rs`,
`crates/lash-trace/src/workflow_overlay.rs`, and
`crates/lash-trace/src/workflow_overlay/tests.rs`.

### R4: durable per-effect summary

Result incorporation records the first `PROCESS_EFFECT_OCCURRENCE_CAP`, 8,
occurrences per effect node, counted across the node's sites. Each contains
node id, its site's occurrence, the site and loop context, operation,
outcome class, optional code, a typed `call_id` for tool effects, and idempotency
key, with no payload, timing, or attempt. The call id joins to the retained
`ToolCallRecord` through `Processes::tool_call(process_id, call_id)`; engine-only
effects carry `None`. Later occurrences contribute to bounded omission counts. Progress and
pending entries travel in the VM snapshot.

Pending occurrences commit at the next process boundary: entering or clearing
a wait, a body event, or terminal completion. They are the prelude of that
boundary's atomic event batch; terminal batches add omissions before the
terminal event. A snapshot carries state and commits no summary itself.
The fold updates the process projection and change clock as one committed batch.

Repeating an equal idempotency key is a no-op. A different payload under it
refuses the batch. Failed summary incorporation is infrastructure failure: the
process resumes from its last committed snapshot, and the failure is not an
error delivered to the program. This does not promise exactly-once external
I/O for an effect whose outcome has not committed. Pure computations, branches, and iterations gain no durable
summary events.

Evidence: `crates/lash-core-execution/src/runtime/process/effect_summary.rs:26`,
`:62`, `:79`, `:153`,
`crates/lash-core-execution/src/runtime/process/validation.rs`, and
`crates/lash-conformance/src/conformance/process_event_batch.rs:1`.

### R5: attempt is telemetry identity only

Attempt distinguishes telemetry observations and trace deduplication. It does
not enter effect idempotency keys, group keys, or admitted operation identities. Process
identity is the minted id, with no additional process-incarnation component.

Evidence: `crates/lash-trace/src/workflow_overlay/model.rs`,
`crates/lash-vm-runtime/src/host_identity.rs:1`, and
`crates/lash-core/src/runtime/observation/process_replay.rs`.

### R6: expose the admitted artifact's identity

`WorkflowGraph::source_identity` is `ModuleArtifact::source_identity`, the
`lash-workflow-source/v4` digest of the linked span-free IR atom stream.
It is not a text digest or serializer spelling. A draft projected from source
has no runtime identity. An admitted artifact supplies the runnable projection
and trace identity. Reconciliation checks a graph against its own canonical
reprojection and reports matches, unmatched nodes, and ambiguity; it does not
match arbitrary revisions semantically.

Evidence: `crates/lash-vm/src/artifact.rs:281`,
`crates/lash-vm/src/workflow_graph/projection.rs:51`, `:79`, and
`crates/lash-vm/src/workflow_graph.rs:455`.

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

Evidence: `crates/lash-vm/src/workflow_graph.rs:665`, `:735`, and
`crates/lash-vm/src/workflow_graph/facets.rs:96`, `:154`.

### R8: the projector lives in the IR crate

`lash_vm::WorkflowGraphProjector` owns projection beside IR, and
`lash_vm::workflow_program_from_graph` owns the inverse. Every admitted
construct projects as a typed region, so neither direction reads statement
text from a dialect. The process engine does not depend on `lash-typescript` for trace skeletons.
`check-workflow-graph-model.sh` guards that dependency boundary.

Evidence: `crates/lash-vm/src/workflow_graph/projection.rs:1`,
`crates/lash-typescript/src/workflow_graph/mod.rs:478`,
`crates/lash-vm-runtime/Cargo.toml`, and
`scripts/check-workflow-graph-model.sh`.

Hosts own transport DTOs and wire compatibility ([ADR 0136](0136-hosts-own-their-wire-contracts.md)).

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
| `WorkflowExecutionOverlay` | `TRACE_SCHEMA_VERSION = 36` | Exact snapshot version before shape | Additive fields tolerated; closed status, occurrence, wait, mismatch, and fact variants refused |
| Durable effect-summary events | `PROCESS_EVENT_VOCABULARY_VERSION = 1` | Fleet read window and registered upcaster before strict payload decode | Unknown summary fields and lifecycle kinds refused; the vocabulary is closed under ADR 0137 |

The owners and fences are in `crates/lash-vm/src/workflow_graph.rs:61`, `:129`,
`crates/lash-vm/src/workflow_graph/facets.rs:12`,
`crates/lash-trace/src/lib.rs:164`, `:187`,
`crates/lash-trace/src/workflow_overlay/model.rs`, and
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
Blanket additive decoding weakens authoritative graph shapes;
compatibility follows each carrier's actual role.

## Consequences

Hosts join maps and run data by structural id, subscribe with gap detection,
fold bounded telemetry, and read durable effect outcomes without storing a
trace history. Carrier owners enforce decode policy and published schemas.
Evidence includes graph carrier laws, trace fold/schema tests, process
observation tests, event-page SQL pins, and event-batch conformance.
Store laws use SQLite file, SQLite memory, and PostgreSQL. Laws run the production runtime over a fault-injecting store with labelled commits, a virtual clock and `SimNodes` ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §14). Upgrade
proofs use synthetic-next.

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
