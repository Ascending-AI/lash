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
//! - **Projection versions.** The trace
//!   schema version gate a live peer or a reader, not parked durable bytes.
//! - **Hash-domain family tags.** A `*_FAMILY_VERSION` (and the frame-key and
//!   journal-identity tags spelled differently) names the preimage family of a
//!   content-addressed identity. It is a component of the identity, not a
//!   stamp any reader compares.
//!
//! # Feature gating is honest, not incidental
//!
//! The Lashlang VM and RLM formats exist only when the `rlm` feature is on,
//! because the crates that define them are optional dependencies. A build
//! without the feature writes none of its formats, so
//! [`durable_formats`] does not list them. Module artifacts are different: their durable surface and
//! semantic identity are owned by non-optional `lash-sansio`, so the format is
//! listed in every build even when the optional verifier is absent.

pub use lash_core::engine::UpgradePolicy;
pub use lash_core::plugin::PLUGIN_ADMISSION_CHECKPOINT_VERSION;
pub use lash_core::session_model::PLUGIN_RUNTIME_EVENT_VERSION;

pub use lash_core::durable_port::domain::WAIT_ROW_FORMAT_VERSION;
pub use lash_core::formats::BuildFormats;
pub use lash_core::formats::RUN_RECORD_FORMAT_VERSION;
pub use lash_core::store::{
    APPEND_REQUEST_IDENTITY_ENCODING_VERSION, CHECKPOINT_COMPONENT_ENCODING_VERSION,
    CREATE_SESSION_REQUEST_IDENTITY_ENCODING_VERSION, CURRENT_SESSION_STATE_VERSION,
    RECORD_CONFIG_REQUEST_IDENTITY_ENCODING_VERSION, RUNTIME_COMMIT_RECEIPT_SCHEMA_VERSION,
    SESSION_CHECKPOINT_SCHEMA_VERSION, SESSION_HEAD_META_SCHEMA_VERSION,
};
pub use lash_core::tool_run::material::OUTCOME_MATERIAL_FORMAT_VERSION;
pub use lash_core::{
    PROCESS_EVENT_VOCABULARY_VERSION, PROCESS_WAKE_DELIVERY_FORMAT_VERSION,
    SCOPE_STORAGE_PAYLOAD_VERSION, SESSION_NODE_BODY_SCHEMA_VERSION,
};
#[cfg(feature = "rlm")]
pub use lash_lashlang_runtime::LASHLANG_SEGMENT_STATE_VERSION;
#[cfg(feature = "rlm")]
pub use lash_protocol_rlm::{
    NATIVE_TRANSPORT_VERSION, RLM_DRIVER_STATE_VERSION, RLM_PROTOCOL_EVENT_VERSION,
    RLM_SNAPSHOT_VERSION,
};
pub use lash_sansio::{LASHLANG_SEMANTIC_HASH_VERSION, TURN_CHECKPOINT_SCHEMA_VERSION};
#[cfg(feature = "rlm")]
pub use lashlang::{
    BYTECODE_FORMAT_VERSION, LASHLANG_SNAPSHOT_VERSION, LASHLANG_VM_ABI_VERSION,
    VM_CONTINUATION_FORMAT_VERSION, WORKFLOW_GRAPH_SCHEMA_VERSION,
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
    /// The opaque plugin admission checkpoint body and native view.
    PluginAdmissionCheckpoint,
    /// A runtime-plugin event nested in session history.
    PluginRuntimeEvent,
    /// An RLM event nested in session history.
    RlmProtocolEvent,
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
    /// The versioned typed parent scope persisted beside a ledger row's index
    /// projection.
    ScopeStoragePayload,
    /// The runtime-owned durable effect-summary events a process's log
    /// carries (`process.effect_outcome`, `process.effect_omissions`).
    ProcessEffectReport,
    /// The identity bytes a retried append request must reproduce. Identity,
    /// not a stored stamp — see [`FormatProbe::IdentityOnly`].
    AppendRequestIdentity,
    /// The identity encoding of a record-config semantic-boundary request.
    RecordConfigRequestIdentity,
    /// The identity encoding of a create-session semantic-boundary request.
    CreateSessionRequestIdentity,
    /// The serialized sans-IO turn checkpoint.
    TurnCheckpoint,
    /// The bodies of a tool round's run records.
    RunRecord,
    /// A wait row: a keyed promise, durable wait or timer.
    WaitRow,
    /// The material of a tool outcome.
    OutcomeMaterial,
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
    /// A lashlang process's engine state: its VM snapshot and the operation
    /// it parked on.
    LashlangSegmentHandover,
    /// The RLM snapshot envelope stored behind a checkpoint component.
    RlmSnapshotEnvelope,
    /// The serialized workflow-graph contract a persisted graph projection
    /// carries.
    WorkflowGraphSchema,
    /// The optional workflow type-facet projection a persisted graph carries.
    WorkflowTypeFacet,
    /// The RLM driver state parked in the protocol driver-state slot.
    RlmDriverState,
    /// The native RLM provider-call and repair envelopes recorded in session
    /// history.
    NativeRlmTransport,
    /// A durable format the build's effect engine registers of its own
    /// (ADR 0104 §2). The facade names no engine: a format whose bytes and
    /// version are the engine's own — its journal, its object state — is
    /// the engine's row to declare, so it arrives through the engine's
    /// registry as this engine-neutral handle rather than as a variant
    /// spelled for the engine.
    Engine(EngineFormat),
    /// The Lashlang VM ABI this build implements. Never persisted — see
    /// [`FormatProbe::NotPersisted`].
    VmAbi,
}

/// A durable format an effect engine registered with the table
/// (ADR 0104 §2) — the engine-neutral handle callers name it through.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub struct EngineFormat {
    /// The opaque id the engine registered the format under.
    pub id: &'static str,
    /// The operator-facing name used in preflight reports.
    pub name: &'static str,
    /// Why no bounded preflight surface enumerates the format, in the
    /// engine's words: the engine's durable state lives outside lash's own
    /// store.
    pub unwalkable_reason: &'static str,
    /// How the format's stored bytes move to a newer build (ADR 0106 §2) —
    /// the engine's declaration, not a facade guess, because it is the
    /// engine's bytes the policy describes.
    pub upgrade_policy: UpgradePolicy,
}

impl DurableFormat {
    /// The operator-facing name used in preflight reports.
    pub fn name(self) -> &'static str {
        match self {
            DurableFormat::PluginAdmissionCheckpoint => "plugin admission checkpoint",
            DurableFormat::PluginRuntimeEvent => "plugin runtime event",
            DurableFormat::RlmProtocolEvent => "RLM protocol event",
            DurableFormat::ModuleArtifact => "module artifact",
            DurableFormat::SessionCheckpointManifest => "session checkpoint manifest",
            DurableFormat::CheckpointComponentEncoding => "checkpoint component encoding",
            DurableFormat::SessionHeadMeta => "session head meta",
            DurableFormat::ProcessWakeDelivery => "process wake delivery",
            DurableFormat::SessionNodeBody => "session node body",
            DurableFormat::SessionStateGeneration => "session state generation",
            DurableFormat::ScopeStoragePayload => "scope storage payload",
            DurableFormat::ProcessEffectReport => "process effect summary",
            DurableFormat::AppendRequestIdentity => "append request identity",
            DurableFormat::RecordConfigRequestIdentity => "record-config request identity",
            DurableFormat::CreateSessionRequestIdentity => "create-session request identity",
            DurableFormat::TurnCheckpoint => "turn checkpoint",
            DurableFormat::RunRecord => "run record",
            DurableFormat::WaitRow => "wait row",
            DurableFormat::OutcomeMaterial => "outcome material",
            DurableFormat::RuntimeCommitReceipt => "runtime commit receipt",
            DurableFormat::Bytecode => "bytecode",
            DurableFormat::VmContinuation => "VM continuation",
            DurableFormat::LashlangSnapshot => "Lashlang snapshot",
            DurableFormat::LashlangSegmentHandover => "Lashlang segment handover",
            DurableFormat::RlmSnapshotEnvelope => "RLM snapshot envelope",
            DurableFormat::WorkflowGraphSchema => "workflow graph schema",
            DurableFormat::WorkflowTypeFacet => "workflow type facet",
            DurableFormat::RlmDriverState => "RLM driver state",
            DurableFormat::NativeRlmTransport => "native RLM transport",
            DurableFormat::Engine(format) => format.name,
            DurableFormat::VmAbi => "Lashlang VM ABI",
        }
    }

    /// How this format's stored bytes move to a newer build (ADR 0106 §2).
    ///
    /// The match is exhaustive — every durable format declares one of the
    /// three policies — and `scripts/check_format_registry.py` holds each
    /// manifest row's answer equal to the `upgrade =` the surface's registry
    /// entry declares (an engine row's field stands in for an arm), so the
    /// answer here cannot drift from the declared upgrade path.
    pub fn upgrade_policy(self) -> UpgradePolicy {
        match self {
            DurableFormat::PluginAdmissionCheckpoint => UpgradePolicy::Migrate,
            DurableFormat::PluginRuntimeEvent => UpgradePolicy::Migrate,
            DurableFormat::RlmProtocolEvent => UpgradePolicy::Migrate,
            DurableFormat::ModuleArtifact => UpgradePolicy::Coexist,
            DurableFormat::SessionCheckpointManifest => UpgradePolicy::Migrate,
            DurableFormat::CheckpointComponentEncoding => UpgradePolicy::Migrate,
            DurableFormat::SessionHeadMeta => UpgradePolicy::Migrate,
            DurableFormat::ProcessWakeDelivery => UpgradePolicy::Migrate,
            DurableFormat::SessionNodeBody => UpgradePolicy::Migrate,
            DurableFormat::SessionStateGeneration => UpgradePolicy::Migrate,
            DurableFormat::ScopeStoragePayload => UpgradePolicy::Migrate,
            DurableFormat::ProcessEffectReport => UpgradePolicy::Migrate,
            DurableFormat::AppendRequestIdentity => UpgradePolicy::Coexist,
            DurableFormat::RecordConfigRequestIdentity => UpgradePolicy::Coexist,
            DurableFormat::CreateSessionRequestIdentity => UpgradePolicy::Coexist,
            DurableFormat::TurnCheckpoint => UpgradePolicy::Drain,
            DurableFormat::RunRecord => UpgradePolicy::Drain,
            DurableFormat::WaitRow => UpgradePolicy::Drain,
            DurableFormat::OutcomeMaterial => UpgradePolicy::Drain,
            DurableFormat::RuntimeCommitReceipt => UpgradePolicy::Migrate,
            DurableFormat::Bytecode => UpgradePolicy::Coexist,
            DurableFormat::VmContinuation => UpgradePolicy::Drain,
            DurableFormat::LashlangSnapshot => UpgradePolicy::Migrate,
            DurableFormat::LashlangSegmentHandover => UpgradePolicy::Drain,
            DurableFormat::RlmSnapshotEnvelope => UpgradePolicy::Migrate,
            DurableFormat::WorkflowGraphSchema => UpgradePolicy::Migrate,
            DurableFormat::WorkflowTypeFacet => UpgradePolicy::Migrate,
            DurableFormat::RlmDriverState => UpgradePolicy::Migrate,
            DurableFormat::NativeRlmTransport => UpgradePolicy::Migrate,
            DurableFormat::Engine(format) => format.upgrade_policy,
            DurableFormat::VmAbi => UpgradePolicy::Drain,
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
/// language substrate, then the protocol envelope above it, then the formats
/// the build's effect engine registers (ADR 0104 §2), so a rendered report
/// reads outward from the store.
pub fn durable_formats() -> impl Iterator<Item = DurableFormatEntry> {
    const FACADE_FORMATS: &[DurableFormatEntry] = &[
        DurableFormatEntry {
            format: DurableFormat::PluginAdmissionCheckpoint,
            version: FormatVersion::Counter(PLUGIN_ADMISSION_CHECKPOINT_VERSION),
            owning_crate: "lash-core-execution",
            constant: "PLUGIN_ADMISSION_CHECKPOINT_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::PluginRuntimeEvent,
            version: FormatVersion::Counter(PLUGIN_RUNTIME_EVENT_VERSION),
            owning_crate: "lash-core-execution",
            constant: "PLUGIN_RUNTIME_EVENT_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::RlmProtocolEvent,
            version: FormatVersion::Counter(RLM_PROTOCOL_EVENT_VERSION),
            owning_crate: "lash-protocol-rlm",
            constant: "RLM_PROTOCOL_EVENT_VERSION",
            probe: FormatProbe::Comparable,
        },
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
            format: DurableFormat::ScopeStoragePayload,
            version: FormatVersion::Counter(SCOPE_STORAGE_PAYLOAD_VERSION as u32),
            owning_crate: "lash-core",
            constant: "SCOPE_STORAGE_PAYLOAD_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::ProcessEffectReport,
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
            format: DurableFormat::TurnCheckpoint,
            version: FormatVersion::Counter(TURN_CHECKPOINT_SCHEMA_VERSION),
            owning_crate: "lash-sansio",
            constant: "TURN_CHECKPOINT_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::RunRecord,
            version: FormatVersion::Counter(RUN_RECORD_FORMAT_VERSION),
            owning_crate: "lash-core-execution",
            constant: "RUN_RECORD_FORMAT_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::WaitRow,
            version: FormatVersion::Counter(WAIT_ROW_FORMAT_VERSION),
            owning_crate: "lash-durable",
            constant: "WAIT_ROW_FORMAT_VERSION",
            probe: FormatProbe::Comparable,
        },
        DurableFormatEntry {
            format: DurableFormat::OutcomeMaterial,
            version: FormatVersion::Counter(OUTCOME_MATERIAL_FORMAT_VERSION as u32),
            owning_crate: "lash-core-store",
            constant: "OUTCOME_MATERIAL_FORMAT_VERSION",
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
            format: DurableFormat::RlmDriverState,
            version: FormatVersion::Counter(RLM_DRIVER_STATE_VERSION),
            owning_crate: "lash-protocol-rlm",
            constant: "RLM_DRIVER_STATE_VERSION",
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
    ];
    FACADE_FORMATS
        .iter()
        .copied()
        .chain(engine_durable_formats())
}

/// A build with no effect engine registers no engine formats.
fn engine_durable_formats() -> impl Iterator<Item = DurableFormatEntry> {
    std::iter::empty()
}

/// The manifest row for one format, when this build carries it.
///
/// `None` means the format is not part of this build — the Lashlang and RLM
/// rows are absent without the `rlm` feature — which is a different answer from
/// "version zero" and is reported as such.
pub fn durable_format(format: DurableFormat) -> Option<DurableFormatEntry> {
    durable_formats().find(|entry| entry.format == format)
}

/// The durable formats actor state holds beyond the runtime core's own
/// (ADR 0106 §1): with `rlm`, the VM continuation and Lashlang snapshot a
/// code cell or a lashlang process resumes from, and the RLM snapshot
/// envelope a cell's snapshot data is. [`DurableBackendBuilder`] adds them to
/// every actor kind's format set, beside the turn checkpoint, run records,
/// wait rows, outcome materials and engine states the core declares.
///
/// [`DurableBackendBuilder`]: crate::durable::DurableBackendBuilder
pub fn actor_state_surfaces() -> Vec<lash_core::durable_port::FormatSurface> {
    #[cfg(feature = "rlm")]
    {
        use lash_core::durable_port::FormatSurface;
        vec![
            FormatSurface::new("vm-continuation", VM_CONTINUATION_FORMAT_VERSION),
            FormatSurface::new("lashlang-snapshot", LASHLANG_SNAPSHOT_VERSION),
            FormatSurface::new("rlm-snapshot", RLM_SNAPSHOT_VERSION),
        ]
    }
    #[cfg(not(feature = "rlm"))]
    {
        Vec::new()
    }
}
