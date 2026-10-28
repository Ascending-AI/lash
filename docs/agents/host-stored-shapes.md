# Shapes hosts persist or mirror

The host guides name the contracts below for host storage, projection, trusted
relays or retained editing. `HOST_STORED_SCHEMA_VERSION` in
[`formats.rs`](../../crates/lash/src/formats.rs) guards their roots and their
reachable serialized fields. It is reported by `durable_formats()` as
`DurableFormat::HostStoredShapes`, with the drain policy. Save the writing
build's witness with host records. Before 1.0, a changed or missing witness
requires recreation or an explicit review against the writing source snapshot.
Lash cannot enumerate the host's product rows during preflight.

| Host storage contract | Roots under the host-storage witness | Documentation authority |
| --- | --- | --- |
| Saved sans-IO turns and referenced environment content | `SavedTurn`, `TurnCheckpoint`, `TurnCheckpointContent`, `SyncedEnvironment` | [FIG-7506](../host-upgrades/unreleased/FIG-7506.md), `SavedTurn` Rust documentation |
| Committed transcript, cells, calls and settled outcomes | `CommittedTurn`, `CommittedTurnWithCalls`, both committed page forms, `TranscriptEntry`, `CellRecord`, `ExecutedCall`, `ExecutedCallOutcome`, `ToolCallRead`, tool/model call addresses and records | [Committed history](../adr/0129-committed-history-is-typed-facts-a-host-renders.md), [settle verdicts](../operations/durable-hosting.md#reading-a-calls-settle-verdict) |
| Check verdicts | `CheckRecord`, `AttributedVerdict`, `BeforeCheckVerdict`, `AfterCheckVerdict`, `BeforeSelection` and their typed causes | [Tool-run contract](../architecture/tool-run-contract.md), the `CheckRecord` record contract |
| Host cursor delivery | `HistoryCursor`, `CommittedTurnCursor`, `ProcessHistoryContinuation`, `ProcessChangeCursor`, `SessionCursor`, `ProcessObservationCursor`, `ChangePage` and `ProcessEventsRead` | [Durable history](../operations/durable-hosting.md), [process observations](../observing-processes.md) |
| Public process projections and trusted internal records | `ProjectedObservedProcess`, `ProjectedProcessEvent`, their lifecycle/terminal projections, `ObservedProcess`, changes and events, event pages and retention outcomes, `ProcessAwaitOutput`, `ToolValue`, outputs, failures, cancellation, controls and `CachedToolSuccess` | [Projection and Durable encodings](../observing-processes.md#one-read-shape), [wire ownership](../adr/0136-hosts-own-their-wire-contracts.md) |
| Workflow and caption views | `WorkflowView`, `CallableView`, `EntryView`, `CaptionView` and all serialized view types in their owning module files | [Workflow hosts](../workflow-hosts.md), [host schema inventory](../../schemas/host/README.md) |
| Execution overlays and retained traces | `ProcessWorkflowOverlay`, `WorkflowExecutionOverlay`, `TraceRecord`, their sites, facts, calls, outcomes and coverage | [Execution overlays](../observing-processes.md#folding-an-execution-overlay), [trace retention](../observing-processes.md) |
| Retained document and editing data | `DocumentBytes`, `SourceMap` and its wire representation, `Captions` and caption targets, `Correspondence`, `Operation`, `Transaction`, `Extraction`, `EditRefusal`, `MaterializeRefusal` | [Opaque document retention](../kernel/document-bytes.md), [workflow hosts](../workflow-hosts.md), [published schemas](../../schemas/host/README.md) |
| Package/publication records | `LinkRecord`, `PackageRecord`, `PublicationRecord`, `NamespaceSnapshot` and their reachable identities and content | [Package ownership and retention](../host-packages.md) |
| Explicit tool-live wire contracts | `ToolLiveItem`, `ToolLiveFilter` and their event/cursor payloads | [ADR 0136](../adr/0136-hosts-own-their-wire-contracts.md) |

Checkpoint roots already had a `TURN_CHECKPOINT_SCHEMA_VERSION` guard. The
cc0e5b332f field change from `Option<SyncedEnvironment>` to
`Option<CheckpointContentRef>` is now a regression case against both that
existing witness and the host-storage witness. The current gate detects it;
the incident alone does not establish that the root was absent.

Cells and tool records also retain their existing RLM, run-record and backend
schema guards. Trace records and workflow execution overlays retain trace
schema 37; the separate host-storage witness tracks their persisted projection
changes without moving their schema filenames or semantic kernel version.

`Serialize`/`Deserialize`, hand-written `DurableSerialize` implementations and
re-exported field adapters are part of the guarded preimage. A raw JSON field
whose producer defines its object shape also guards that producer, such as the
process projection methods and tool-value projection methods.

Host-owned DTOs, custom protocol associated types, transient handles and
accumulators are outside this inventory. A serialized snapshot returned by an
accumulator is inside it. Internal durable rows retain the backend, record,
page and identity witnesses declared by their owners; this witness additionally
covers the host-facing records above.
