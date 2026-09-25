//! Every durable format version this build writes, in one typed table.
//!
//! Lash's durable formats fail closed (ADR 0055/0061/0064): there is no
//! migration decoder at any of these boundaries, so state parked by another
//! build is refused rather than read. That makes the version integers
//! operationally load-bearing — they are what an operator compares before a
//! deploy, and what a refusal names afterwards — and until now most of them
//! were unreachable from a host at all. Four were private to their own module.
//!
//! The table is **exhaustive over the durable formats this build writes**, which
//! is a stronger claim than "the ones that are easy to report" and the reason
//! [`FormatVersion`] distinguishes a comparable counter from an opaque build
//! identity rather than collapsing both into one field.
//!
//! **This module re-exports; it does not redefine.** Each constant below is the
//! same symbol the code that writes the format uses, lifted through its owning
//! crate. Drift between "the version the runtime writes" and "the version the
//! manifest reports" is therefore not a bug that can be introduced: there is
//! one definition and the compiler resolves it. A central table of copied
//! integers would have exactly that bug, which is why this is not one.
//!
//! # What is *not* here
//!
//! The registry this table is checked against is
//! `scripts/versioned-surfaces.toml`: every version-shaped constant in the
//! repository is a registered surface there or an exclusion with a stated
//! reason, and every registered surface either names its row here or says why
//! it has none. `scripts/check_format_registry.py` fails CI when the two
//! disagree, which is what keeps the exhaustiveness claim above true. The
//! exclusions fall into these classes:
//!
//! - **Store schema versions.** SQLite's four `user_version` stamps and
//!   PostgreSQL's component stamp belong to their backends, version on their
//!   own cadence, and are read from the deployment rather than from the build.
//!   They come back from a backend's
//!   [`StorePreflight`](crate::persistence::StorePreflight) instead.
//! - **Wire protocol versions.** `REMOTE_PROTOCOL_VERSION` and the trace
//!   schema version gate a live peer or a reader, not parked durable bytes.
//! - **Hash-domain family tags.** A `*_FAMILY_VERSION` (and the frame-key and
//!   journal-identity tags spelled differently) names the preimage family of a
//!   content-addressed identity. It is a component of the identity, not a
//!   stamp any reader compares.
//!
//! # Feature gating is honest, not incidental
//!
//! The Lashlang VM and RLM formats exist only when the `rlm` feature is on,
//! and the Restate journal and object-state formats only when the `restate`
//! feature is on, because the crates that define them are optional
//! dependencies. A build without a feature writes none of its formats, so
//! [`durable_formats`] does not list them. Module artifacts are different: their durable surface and
//! semantic identity are owned by non-optional `lash-sansio`, so the format is
//! listed in every build even when the optional verifier is absent.

pub use lash_core::facade_support::PROCESS_LEASE_SCHEMA_VERSION;
pub use lash_core::store::{
    APPEND_REQUEST_IDENTITY_ENCODING_VERSION, CHECKPOINT_COMPONENT_ENCODING_VERSION,
    CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION, CURRENT_SESSION_STATE_VERSION,
    RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION, RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION,
    SESSION_CHECKPOINT_SCHEMA_VERSION, SESSION_HEAD_META_SCHEMA_VERSION,
    USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION,
};
pub use lash_core::{
    PARENT_SCOPE_STORAGE_PAYLOAD_VERSION, PROCESS_EVENT_VOCABULARY_VERSION,
    PROCESS_WAKE_DELIVERY_FORMAT_VERSION, PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION,
    SESSION_NODE_BODY_SCHEMA_VERSION, TOOL_ATTEMPT_CAPTURE_VERSION, TOOL_CHILD_REQUEST_VERSION,
    TOOL_PRESENTATION_VERSION, TOOL_SETTLEMENT_VERSION,
};
#[cfg(feature = "rlm")]
pub use lash_lashlang_runtime::LASHLANG_SEGMENT_STATE_VERSION;
#[cfg(feature = "rlm")]
pub use lash_protocol_rlm::{
    NATIVE_DRIVER_STATE_VERSION, NATIVE_TRANSPORT_VERSION, RLM_SNAPSHOT_VERSION,
};
#[cfg(feature = "restate")]
pub use lash_restate::{
    DURABLE_WAIT_INDEX_IDENTITY_EPOCH, DURABLE_WAIT_REQUEST_VERSION,
    EFFECT_GROUP_INDEX_PROTOCOL_VERSION, EFFECT_JOURNAL_VERSION, LASH_SESSION_DRIVE_VERSION,
    PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION, RESTATE_PROCESS_JOURNAL_VERSION,
};
pub use lash_sansio::{LASHLANG_SEMANTIC_HASH_VERSION, TURN_CHECKPOINT_SCHEMA_VERSION};
#[cfg(feature = "rlm")]
pub use lashlang::{
    BYTECODE_FORMAT_VERSION, HEAP_SIZE_SCHEDULE_VERSION, LASHLANG_SNAPSHOT_VERSION,
    LASHLANG_VM_ABI_VERSION, VM_CONTINUATION_FORMAT_VERSION, WORKFLOW_GRAPH_SCHEMA_VERSION,
    WORKFLOW_TYPE_FACET_SCHEMA_VERSION,
};

/// One durable format whose version decides whether stored bytes open under
/// this build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DurableFormat {
    /// The identity-verified persisted Lashlang/TypeScript module artifact.
    ModuleArtifact,
    /// The session checkpoint manifest: the keyed set of components a
    /// checkpoint root names.
    SessionCheckpointManifest,
    /// The encoding of the logical bytes stored behind a checkpoint component
    /// descriptor.
    CheckpointComponentEncoding,
    /// The persisted session-head metadata payload.
    SessionHeadMeta,
    /// The serialized wake-delivery payload a process outbox row carries.
    ProcessWakeDelivery,
    /// The persisted JSON body of a session-graph node.
    SessionNodeBody,
    /// The session-state generation marker admission compares before any
    /// mutable session payload is read (ADR 0077).
    SessionStateGeneration,
    /// The persisted protocol-specific turn-options envelope.
    ProtocolTurnOptions,
    /// The versioned typed parent scope persisted beside a ledger row's index
    /// projection.
    ParentScopeStoragePayload,
    /// The persisted process lease record.
    ProcessLease,
    /// The runtime-owned durable effect-summary events a process's log
    /// carries (`process.effect_outcome`, `process.effect_omissions`).
    ProcessEffectSummary,
    /// The identity bytes a retried append request must reproduce. Identity,
    /// not a stored stamp — see [`FormatProbe::IdentityOnly`].
    AppendRequestIdentity,
    /// The identity encoding of a record-config semantic-boundary request.
    RecordConfigRequestIdentity,
    /// The identity encoding of a create-session semantic-boundary request.
    CreateSessionRequestIdentity,
    /// The identity encoding of a usage-ledger semantic-boundary request.
    UsageLedgerRequestIdentity,
    /// The retained tool-child request of a durable effect group.
    ToolChildRequest,
    /// The semantic settlement a tool child journals on its outcome.
    ToolSettlement,
    /// The facts one atomic tool attempt journals with its outcome.
    ToolAttemptCapture,
    /// The journaled presentation record of one tool result.
    ToolPresentation,
    /// The serialized sans-IO turn checkpoint.
    TurnCheckpoint,
    /// The persisted runtime turn-commit receipt a committed turn replays
    /// from `runtime_turn_commits.result_json`.
    RuntimeCommitReceipt,
    /// Compiled Lashlang bytecode. Identity-checked rather than
    /// version-compared — see [`FormatProbe::IdentityOnly`].
    Bytecode,
    /// The durable VM-continuation envelope a parked Lashlang segment carries.
    VmContinuation,
    /// The canonical Lashlang execution snapshot.
    LashlangSnapshot,
    /// The Lashlang heap size schedule, which rides in the continuation.
    HeapSizeSchedule,
    /// The Restate Lashlang segment-handover envelope.
    LashlangSegmentHandover,
    /// The RLM snapshot envelope stored behind a checkpoint component.
    RlmSnapshotEnvelope,
    /// The serialized workflow-graph contract a persisted graph projection
    /// carries.
    WorkflowGraphSchema,
    /// The optional workflow type-facet projection a persisted graph carries.
    WorkflowTypeFacet,
    /// The native RLM driver state parked in the protocol driver-state slot.
    NativeRlmDriverState,
    /// The native RLM provider-call and repair envelopes recorded in session
    /// history.
    NativeRlmTransport,
    /// The Restate durable-wait nested workflow request journaled by a
    /// Restate deployment.
    RestateDurableWaitRequest,
    /// The identity epoch a Restate durable-wait index stamps into its object
    /// state.
    RestateDurableWaitIndexEpoch,
    /// The Restate-journaled process-command admission payload.
    RestateProcessCommandJournal,
    /// The Restate effect-group protocol a group's index stamps into its
    /// object state.
    RestateEffectGroupIndexProtocol,
    /// The leading commands every Restate process-segment invocation
    /// journals: segment admission (FIG-3588).
    RestateProcessJournal,
    /// The generation stamped into every recorded effect's Restate journal
    /// entry (ADR 0105 §12).
    RestateEffectJournal,
    /// The leading commands of the Restate session driver's handlers, stamped
    /// on every `LashSession` and `LashTurn` request (ADR 0105 §12, FIG-3600).
    RestateSessionDrive,
    /// The Lashlang VM ABI this build implements. Never persisted — see
    /// [`FormatProbe::NotPersisted`].
    VmAbi,
}

impl DurableFormat {
    /// The operator-facing name used in preflight reports.
    pub fn name(self) -> &'static str {
        match self {
            DurableFormat::ModuleArtifact => "module artifact",
            DurableFormat::SessionCheckpointManifest => "session checkpoint manifest",
            DurableFormat::CheckpointComponentEncoding => "checkpoint component encoding",
            DurableFormat::SessionHeadMeta => "session head meta",
            DurableFormat::ProcessWakeDelivery => "process wake delivery",
            DurableFormat::SessionNodeBody => "session node body",
            DurableFormat::SessionStateGeneration => "session state generation",
            DurableFormat::ProtocolTurnOptions => "protocol turn options",
            DurableFormat::ParentScopeStoragePayload => "parent scope storage payload",
            DurableFormat::ProcessLease => "process lease",
            DurableFormat::ProcessEffectSummary => "process effect summary",
            DurableFormat::AppendRequestIdentity => "append request identity",
            DurableFormat::RecordConfigRequestIdentity => "record-config request identity",
            DurableFormat::CreateSessionRequestIdentity => "create-session request identity",
            DurableFormat::UsageLedgerRequestIdentity => "usage-ledger request identity",
            DurableFormat::ToolChildRequest => "tool child request",
            DurableFormat::ToolSettlement => "tool settlement",
            DurableFormat::ToolAttemptCapture => "tool attempt capture",
            DurableFormat::ToolPresentation => "tool presentation",
            DurableFormat::TurnCheckpoint => "turn checkpoint",
            DurableFormat::RuntimeCommitReceipt => "runtime commit receipt",
            DurableFormat::Bytecode => "bytecode",
            DurableFormat::VmContinuation => "VM continuation",
            DurableFormat::LashlangSnapshot => "Lashlang snapshot",
            DurableFormat::HeapSizeSchedule => "heap size schedule",
            DurableFormat::LashlangSegmentHandover => "Lashlang segment handover",
            DurableFormat::RlmSnapshotEnvelope => "RLM snapshot envelope",
            DurableFormat::WorkflowGraphSchema => "workflow graph schema",
            DurableFormat::WorkflowTypeFacet => "workflow type facet",
            DurableFormat::NativeRlmDriverState => "native RLM driver state",
            DurableFormat::NativeRlmTransport => "native RLM transport",
            DurableFormat::RestateDurableWaitRequest => "Restate durable-wait request",
            DurableFormat::RestateDurableWaitIndexEpoch => "Restate durable-wait index epoch",
            DurableFormat::RestateProcessCommandJournal => "Restate process-command journal",
            DurableFormat::RestateEffectGroupIndexProtocol => "Restate effect-group index protocol",
            DurableFormat::RestateProcessJournal => "Restate process journal prefix",
            DurableFormat::RestateEffectJournal => "Restate effect journal",
            DurableFormat::RestateSessionDrive => "Restate session drive prefix",
            DurableFormat::VmAbi => "Lashlang VM ABI",
        }
    }
}

/// A format's version, which is a counter for most formats and a build string
/// for the VM ABI.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum FormatVersion {
    /// A monotonic counter. A found value that is not this one is refused in
    /// both directions; the boundary is exact-match, not minimum.
    Counter(u32),
    /// An opaque build identity compared for equality.
    Identity(&'static str),
}

impl std::fmt::Display for FormatVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatVersion::Counter(version) => write!(f, "{version}"),
            FormatVersion::Identity(identity) => write!(f, "{identity}"),
        }
    }
}

/// How a readability probe can treat a format — the honest limit of what
/// stored bytes can be asked.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum FormatProbe {
    /// Stored bytes carry their own version, so a probe reads it and compares.
    Comparable,
    /// Stored bytes carry an identity, not a version: nothing recoverable says
    /// "this was written by version N". A probe can only recompute the
    /// identity this build would produce and check whether it matches, which
    /// costs a per-item recompute and so belongs to the deep walk.
    IdentityOnly,
    /// Never persisted. There is no stored counterpart to compare, so a probe
    /// reports the build's value informationally and compares nothing. Claiming
    /// a verdict here would be claiming evidence that does not exist.
    NotPersisted,
}

/// One row of the manifest.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DurableFormatEntry {
    /// Which format this row describes.
    pub format: DurableFormat,
    /// The version this build writes.
    pub version: FormatVersion,
    /// The crate that owns the constant, so a reader can find the definition.
    pub owning_crate: &'static str,
    /// The constant's name, so a refusal can be traced to source.
    pub constant: &'static str,
    /// What a readability probe can conclude about this format.
    pub probe: FormatProbe,
}

/// Every durable format this build writes, with the version it writes.
///
/// The order is stable and report-shaped: store-owned formats first, then the
/// language substrate, then the protocol envelope above it, then the Restate
/// adapter's journal and object state, so a rendered report reads outward from
/// the store.
pub fn durable_formats() -> &'static [DurableFormatEntry] {
    &[
        DurableFormatEntry {
            format: DurableFormat::ModuleArtifact,
            version: FormatVersion::Identity(LASHLANG_SEMANTIC_HASH_VERSION),
            owning_crate: "lash-sansio",
            constant: "LASHLANG_SEMANTIC_HASH_VERSION",
            probe: FormatProbe::IdentityOnly,
        },
        DurableFormatEntry {
            format: DurableFormat::SessionCheckpointManifest,
            version: FormatVersion::Counter(SESSION_CHECKPOINT_SCHEMA_VERSION),
            owning_crate: "lash-core",
            constant: "SESSION_CHECKPOINT_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::CheckpointComponentEncoding,
            version: FormatVersion::Counter(CHECKPOINT_COMPONENT_ENCODING_VERSION),
            owning_crate: "lash-core",
            constant: "CHECKPOINT_COMPONENT_ENCODING_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::SessionHeadMeta,
            version: FormatVersion::Counter(SESSION_HEAD_META_SCHEMA_VERSION),
            owning_crate: "lash-core",
            constant: "SESSION_HEAD_META_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::ProcessWakeDelivery,
            version: FormatVersion::Counter(PROCESS_WAKE_DELIVERY_FORMAT_VERSION),
            owning_crate: "lash-core",
            constant: "PROCESS_WAKE_DELIVERY_FORMAT_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::SessionNodeBody,
            version: FormatVersion::Counter(SESSION_NODE_BODY_SCHEMA_VERSION),
            owning_crate: "lash-core",
            constant: "SESSION_NODE_BODY_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::SessionStateGeneration,
            version: FormatVersion::Counter(CURRENT_SESSION_STATE_VERSION),
            owning_crate: "lash-core",
            constant: "CURRENT_SESSION_STATE_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::ProtocolTurnOptions,
            version: FormatVersion::Counter(PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION),
            owning_crate: "lash-core",
            constant: "PROTOCOL_TURN_OPTIONS_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::ParentScopeStoragePayload,
            version: FormatVersion::Counter(PARENT_SCOPE_STORAGE_PAYLOAD_VERSION as u32),
            owning_crate: "lash-core",
            constant: "PARENT_SCOPE_STORAGE_PAYLOAD_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::ProcessLease,
            version: FormatVersion::Counter(PROCESS_LEASE_SCHEMA_VERSION),
            owning_crate: "lash-core",
            constant: "PROCESS_LEASE_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::ProcessEffectSummary,
            version: FormatVersion::Counter(PROCESS_EVENT_VOCABULARY_VERSION),
            owning_crate: "lash-core",
            constant: "PROCESS_EVENT_VOCABULARY_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::AppendRequestIdentity,
            version: FormatVersion::Counter(APPEND_REQUEST_IDENTITY_ENCODING_VERSION),
            owning_crate: "lash-core",
            constant: "APPEND_REQUEST_IDENTITY_ENCODING_VERSION",
            probe: FormatProbe::IdentityOnly,
        },
        DurableFormatEntry {
            format: DurableFormat::RecordConfigRequestIdentity,
            version: FormatVersion::Counter(RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION),
            owning_crate: "lash-core",
            constant: "RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION",
            probe: FormatProbe::IdentityOnly,
        },
        DurableFormatEntry {
            format: DurableFormat::CreateSessionRequestIdentity,
            version: FormatVersion::Counter(CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION),
            owning_crate: "lash-core",
            constant: "CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION",
            probe: FormatProbe::IdentityOnly,
        },
        DurableFormatEntry {
            format: DurableFormat::UsageLedgerRequestIdentity,
            version: FormatVersion::Counter(USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION),
            owning_crate: "lash-core",
            constant: "USAGE_LEDGER_REQUEST_IDENTITY_ENCODING_VERSION",
            probe: FormatProbe::IdentityOnly,
        },
        DurableFormatEntry {
            format: DurableFormat::ToolChildRequest,
            version: FormatVersion::Counter(TOOL_CHILD_REQUEST_VERSION as u32),
            owning_crate: "lash-core",
            constant: "TOOL_CHILD_REQUEST_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::ToolSettlement,
            version: FormatVersion::Counter(TOOL_SETTLEMENT_VERSION as u32),
            owning_crate: "lash-core",
            constant: "TOOL_SETTLEMENT_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::ToolAttemptCapture,
            version: FormatVersion::Counter(TOOL_ATTEMPT_CAPTURE_VERSION as u32),
            owning_crate: "lash-core",
            constant: "TOOL_ATTEMPT_CAPTURE_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::ToolPresentation,
            version: FormatVersion::Counter(TOOL_PRESENTATION_VERSION as u32),
            owning_crate: "lash-core",
            constant: "TOOL_PRESENTATION_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::TurnCheckpoint,
            version: FormatVersion::Counter(TURN_CHECKPOINT_SCHEMA_VERSION),
            owning_crate: "lash-sansio",
            constant: "TURN_CHECKPOINT_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::RuntimeCommitReceipt,
            version: FormatVersion::Counter(RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION),
            owning_crate: "lash-core",
            constant: "RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::Bytecode,
            version: FormatVersion::Counter(BYTECODE_FORMAT_VERSION),
            owning_crate: "lashlang",
            constant: "BYTECODE_FORMAT_VERSION",
            probe: FormatProbe::IdentityOnly,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::VmContinuation,
            version: FormatVersion::Counter(VM_CONTINUATION_FORMAT_VERSION),
            owning_crate: "lashlang",
            constant: "VM_CONTINUATION_FORMAT_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::LashlangSnapshot,
            version: FormatVersion::Counter(LASHLANG_SNAPSHOT_VERSION),
            owning_crate: "lashlang",
            constant: "LASHLANG_SNAPSHOT_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::HeapSizeSchedule,
            version: FormatVersion::Counter(HEAP_SIZE_SCHEDULE_VERSION),
            owning_crate: "lashlang",
            constant: "HEAP_SIZE_SCHEDULE_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::LashlangSegmentHandover,
            version: FormatVersion::Counter(LASHLANG_SEGMENT_STATE_VERSION),
            owning_crate: "lash-lashlang-runtime",
            constant: "LASHLANG_SEGMENT_STATE_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::RlmSnapshotEnvelope,
            version: FormatVersion::Counter(RLM_SNAPSHOT_VERSION),
            owning_crate: "lash-protocol-rlm",
            constant: "RLM_SNAPSHOT_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::WorkflowGraphSchema,
            version: FormatVersion::Counter(WORKFLOW_GRAPH_SCHEMA_VERSION),
            owning_crate: "lashlang",
            constant: "WORKFLOW_GRAPH_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::WorkflowTypeFacet,
            version: FormatVersion::Counter(WORKFLOW_TYPE_FACET_SCHEMA_VERSION),
            owning_crate: "lashlang",
            constant: "WORKFLOW_TYPE_FACET_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::NativeRlmDriverState,
            version: FormatVersion::Counter(NATIVE_DRIVER_STATE_VERSION),
            owning_crate: "lash-protocol-rlm",
            constant: "NATIVE_DRIVER_STATE_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::NativeRlmTransport,
            version: FormatVersion::Counter(NATIVE_TRANSPORT_VERSION),
            owning_crate: "lash-protocol-rlm",
            constant: "NATIVE_TRANSPORT_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::VmAbi,
            version: FormatVersion::Identity(LASHLANG_VM_ABI_VERSION),
            owning_crate: "lashlang",
            constant: "LASHLANG_VM_ABI_VERSION",
            probe: FormatProbe::NotPersisted,
        },
        #[cfg(feature = "restate")]
        DurableFormatEntry {
            format: DurableFormat::RestateDurableWaitRequest,
            version: FormatVersion::Counter(DURABLE_WAIT_REQUEST_VERSION as u32),
            owning_crate: "lash-restate",
            constant: "DURABLE_WAIT_REQUEST_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "restate")]
        DurableFormatEntry {
            format: DurableFormat::RestateDurableWaitIndexEpoch,
            version: FormatVersion::Counter(DURABLE_WAIT_INDEX_IDENTITY_EPOCH as u32),
            owning_crate: "lash-restate",
            constant: "DURABLE_WAIT_INDEX_IDENTITY_EPOCH",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "restate")]
        DurableFormatEntry {
            format: DurableFormat::RestateProcessCommandJournal,
            version: FormatVersion::Counter(PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION),
            owning_crate: "lash-restate",
            constant: "PROCESS_COMMAND_JOURNAL_PAYLOAD_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "restate")]
        DurableFormatEntry {
            format: DurableFormat::RestateEffectGroupIndexProtocol,
            version: FormatVersion::Counter(EFFECT_GROUP_INDEX_PROTOCOL_VERSION),
            owning_crate: "lash-restate",
            constant: "EFFECT_GROUP_INDEX_PROTOCOL_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "restate")]
        DurableFormatEntry {
            format: DurableFormat::RestateProcessJournal,
            version: FormatVersion::Counter(RESTATE_PROCESS_JOURNAL_VERSION),
            owning_crate: "lash-restate",
            constant: "RESTATE_PROCESS_JOURNAL_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "restate")]
        DurableFormatEntry {
            format: DurableFormat::RestateEffectJournal,
            version: FormatVersion::Counter(EFFECT_JOURNAL_VERSION),
            owning_crate: "lash-restate",
            constant: "EFFECT_JOURNAL_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "restate")]
        DurableFormatEntry {
            format: DurableFormat::RestateSessionDrive,
            version: FormatVersion::Counter(LASH_SESSION_DRIVE_VERSION),
            owning_crate: "lash-restate",
            constant: "LASH_SESSION_DRIVE_VERSION",
            probe: FormatProbe::Comparable,
        },
    ]
}

/// The manifest row for one format, when this build carries it.
///
/// `None` means the format is not part of this build — the Lashlang and RLM
/// rows are absent without the `rlm` feature and the Restate rows without
/// `restate` — which is a different answer from
/// "version zero" and is reported as such.
pub fn durable_format(format: DurableFormat) -> Option<&'static DurableFormatEntry> {
    durable_formats()
        .iter()
        .find(|entry| entry.format == format)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_manifest_reports_the_constants_the_runtime_writes() {
        // Not a tautology: it pins that the table's rows are the re-exported
        // symbols rather than copied integers, which is the property that makes
        // drift impossible.
        let entry = durable_format(DurableFormat::ProcessWakeDelivery)
            .expect("wake delivery is in every build");
        assert_eq!(
            entry.version,
            FormatVersion::Counter(lash_core::PROCESS_WAKE_DELIVERY_FORMAT_VERSION)
        );
        assert_eq!(entry.probe, FormatProbe::Comparable);
    }

    #[test]
    fn no_format_is_listed_twice() {
        let mut seen = std::collections::BTreeSet::new();
        for entry in durable_formats() {
            assert!(
                seen.insert(entry.format),
                "{:?} is listed more than once",
                entry.format
            );
        }
    }

    #[test]
    fn the_store_owned_formats_are_present_without_any_optional_feature() {
        assert!(durable_format(DurableFormat::ModuleArtifact).is_some());
        for format in [
            DurableFormat::SessionCheckpointManifest,
            DurableFormat::CheckpointComponentEncoding,
            DurableFormat::SessionHeadMeta,
            DurableFormat::ProcessWakeDelivery,
            DurableFormat::SessionNodeBody,
        ] {
            assert!(durable_format(format).is_some(), "{format:?}");
        }
    }

    #[cfg(feature = "rlm")]
    #[test]
    fn the_two_exclusions_are_marked_rather_than_omitted() {
        // Omitting them would make the report silently narrower than the
        // boundary; marking them keeps the limit visible.
        assert_eq!(
            durable_format(DurableFormat::Bytecode).map(|entry| entry.probe),
            Some(FormatProbe::IdentityOnly)
        );
        assert_eq!(
            durable_format(DurableFormat::VmAbi).map(|entry| entry.probe),
            Some(FormatProbe::NotPersisted)
        );
    }

    #[cfg(not(feature = "rlm"))]
    #[test]
    fn a_build_without_the_language_reports_no_language_formats() {
        assert!(durable_format(DurableFormat::VmContinuation).is_none());
        assert!(durable_format(DurableFormat::RlmSnapshotEnvelope).is_none());
    }

    #[test]
    fn the_module_artifact_identity_version_is_pinned_to_its_literal() {
        // `LASHLANG_SEMANTIC_HASH_VERSION` is mixed into every module-artifact
        // hash, so moving it invalidates every stored artifact ref. Nothing
        // asserted the literal -- the constant's only occurrence in the tree
        // was its own definition -- so a bump rode along with whatever change
        // happened to touch it, silently. Pinned here beside the VM ABI
        // literal, which already works this way: a deliberate bump edits this
        // line and says why in the diff.
        assert_eq!(
            durable_format(DurableFormat::ModuleArtifact)
                .expect("the module-artifact format is always reported")
                .version,
            FormatVersion::Identity("lashlang-semantic-v23")
        );
    }

    #[test]
    fn a_version_renders_as_the_operator_would_compare_it() {
        assert_eq!(FormatVersion::Counter(8).to_string(), "8");
        assert_eq!(
            FormatVersion::Identity("lashlang-vm-abi-v6").to_string(),
            "lashlang-vm-abi-v6"
        );
    }

    #[test]
    fn the_session_node_body_is_an_exact_match_counter() {
        // Immutable history used to carry a forward-only fence so an older
        // generation still loaded. Under the store-version window the boundary
        // is exact-match like every other versioned format, and the manifest
        // must say so rather than tell an operator that rolling back is the
        // supported direction.
        let entry = durable_format(DurableFormat::SessionNodeBody)
            .expect("graph node bodies are in every build");
        assert_eq!(
            entry.version,
            FormatVersion::Counter(lash_core::SESSION_NODE_BODY_SCHEMA_VERSION)
        );
        assert_eq!(entry.owning_crate, "lash-core");
    }
}
