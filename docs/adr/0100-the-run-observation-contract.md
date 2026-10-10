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
rendering, interaction, and mutation commands. The kernel document
([ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md)) is the authoring
document; a run view combines its static map with observations and durable
semantic history. Live trace publication is best-effort and cannot fail a
process commit. It is not a durable trace history.

### R0: one structural site identity

A node is named by its site: the unit it belongs to (`main`, a declared
function by name, or a library body by identity) and the chain of child
indexes that reaches it from that unit's body (`K-ID-004`). The machine
reports, and a parked run saves, only sites (`K-SITE`); a node id names a
node within one derivation of one document and is never saved. A site holds
no document hash, so editing one statement keeps the sites of the others in
other units, and a published edit answers a correspondence from every
surviving node's old site to its new one (`K-EDIT-004`).

Version identity is the document's: the SHA-256 of its canonical form
(`K-ID-001`). Effect addressing has a separate owner. A run's effects are
named by their admitted identity (task, site, occurrence, loops) through
`CodeCallIdentities`, which commits with the park
([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §8);
telemetry attempt does not name external effects. A process opener contains
the minted process id. Editing a node cannot weaken that admitted identity.

Evidence: `crates/lash-kernel-doc/src/document.rs`,
`crates/lash-kernel-check`, `crates/lash-vm-broker/src/identity.rs` and
`crates/lash-vm-runtime/src/process/trace.rs`.

### R1: the document carries the site

Every derived fact (node ids, edges, scopes, type facets, effect sets,
execution sites) is a function of the document and the definitions it
reaches (`K-ADM-009`); nothing stores it as authority. An execution's start
names its document by reference (`WorkflowDocumentRef`: the document identity
and the entry) and carries no copy of it; events name exact sites, and a host
looks labels up in the document's annotations (`K-DOC-007`). A host needs no
pairing table or secondary id. The process and RLM emitters name the same
document contract.

Evidence: `crates/lash-trace/src/language_execution.rs`,
`crates/lash-vm-runtime/src/process/documents.rs`, and
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

The snapshot combines a live overlay at its publication boundary with status and
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
pending entries travel in the parked run.

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

### R6: expose the admitted document's identity

A document's identity is the hash of its content without annotations
(`K-ID-001`). It is not a text digest or a dialect's spelling: two sources
that lower to the same document share it. A draft has no runtime identity
until it is published and admitted. A host reconciles a document with an
edited one through the correspondence the edit published, never by matching
revisions semantically.

Evidence: `crates/lash-kernel-doc/src/document.rs` and
`crates/lash-kernel-edit`.

### R7: documents hold kernel forms

A document's expressions are kernel forms. A dialect lowers edited source
into a document and prints a document as source; canonical text is derived.
A host edits through typed transactions whose payloads are kernel forms,
types and sites (`K-EDIT-010`). Type facets are derived, read-only facts.
Admission names every fault with the node it is at (`K-ADM`).

Evidence: `docs/kernel/semantics.md` and `crates/lash-kernel-check`.

### R8: the document needs no dialect

No reader of a document links a dialect: projection, the overlay fold, the
process engine and edits read the kernel crates only
([ADR 0139](0139-the-lash-vm-is-a-dialect-free-kernel.md)), and
`scripts/check-kernel-boundary.py` refuses a language-neutral crate that
links a dialect.

Evidence: `crates/lash-trace/src/workflow_overlay.rs`,
`crates/lash-vm-runtime/Cargo.toml`, and `scripts/check-kernel-boundary.py`.

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
| Kernel document | `KERNEL_VERSION` in the manifest, `KERNEL_DOCUMENT_SCHEMA_VERSION` in the store | Version checked before shape against the fleet read window; derived facts regenerate rather than upcast | Every object closed (`K-DOC-004`) |
| Trace records and events | `TRACE_SCHEMA_VERSION = 36` | Exact trace record version before event decode | Additive known fields tolerated; closed variants refused |
| `WorkflowExecutionOverlay` | `TRACE_SCHEMA_VERSION = 36` | Exact snapshot version before shape | Additive fields tolerated; closed status, occurrence, wait, mismatch, and fact variants refused |
| Durable effect-summary events | `PROCESS_EVENT_VOCABULARY_VERSION = 1` | Fleet read window and registered upcaster before strict payload decode | Unknown summary fields and lifecycle kinds refused; the vocabulary is closed under ADR 0137 |

The owners and fences are in `crates/lash-kernel-doc/src/document.rs`,
`crates/lash-vm-runtime/src/formats.rs`,
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
live gaps remain explicit. Document hashes in sites would remint
unrelated nodes after edits; executable identity belongs on the document.
Blanket additive decoding weakens authoritative document shapes;
compatibility follows each carrier's actual role.

## Consequences

Hosts join maps and run data by structural id, subscribe with gap detection,
fold bounded telemetry, and read durable effect outcomes without storing a
trace history. Carrier owners enforce decode policy and published schemas.
Evidence includes the kernel corpus, trace fold/schema tests, process
observation tests, event-page SQL pins, and event-batch conformance.
Store laws use SQLite file, SQLite memory, and PostgreSQL. Laws run the production runtime over a fault-injecting store with labelled commits, a virtual clock and `SimNodes` ([ADR 0132](0132-durability-is-state-first-over-the-lash-store.md) §14). Upgrade
proofs use synthetic-next.

## Model usage

Usage is data on the model call's recorded result. Hosts meter spend at the
`Provider` seam under [ADR 0127](0127-usage-is-result-data-hosts-meter-spend.md).
Lash has no accounting ledger or delivery dependency.

[ADR 0137](0137-the-host-owns-events-routing-and-scheduling.md) owns host events, routing and scheduling.
