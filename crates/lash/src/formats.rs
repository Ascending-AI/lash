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
//! The Lash VM and RLM formats exist only when the `rlm` feature is on,
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
    PROCESS_EVENT_VOCABULARY_VERSION, SCOPE_STORAGE_PAYLOAD_VERSION,
    SESSION_NODE_BODY_SCHEMA_VERSION,
};
#[cfg(feature = "rlm")]
pub use lash_protocol_rlm::{
    RLM_DRIVER_STATE_VERSION, RLM_PROTOCOL_EVENT_VERSION, RLM_SNAPSHOT_VERSION,
};
pub use lash_sansio::TURN_CHECKPOINT_SCHEMA_VERSION;
#[cfg(feature = "rlm")]
pub use lash_vm_runtime::{
    KERNEL_DOCUMENT_SCHEMA_VERSION, KERNEL_PARKED_STATE_VERSION, KERNEL_SAVED_FUNCTION_VERSION,
    LASH_KERNEL_VERSION,
};

/// One durable format whose version decides whether stored bytes open under
/// this build.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
#[non_exhaustive]
pub enum DurableFormat {
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
    /// The JSON encoding of a kernel document, as a store holds it.
    KernelDocument,
    /// A parked kernel run: the state a code cell or a process body resumes
    /// from, stamped with the kernel version it was parked under.
    KernelParkedState,
    /// A saved function: a function a session keeps between cells, held in
    /// the RLM snapshot and given to a session at its creation.
    KernelSavedFunction,
    /// The RLM snapshot envelope stored behind a checkpoint component.
    RlmSnapshotEnvelope,
    /// The RLM driver state parked in the protocol driver-state slot.
    RlmDriverState,
    /// A durable format the build's effect engine registers of its own
    /// (ADR 0104 §2). The facade names no engine: a format whose bytes and
    /// version are the engine's own — its journal, its object state — is
    /// the engine's row to declare, so it arrives through the engine's
    /// registry as this engine-neutral handle rather than as a variant
    /// spelled for the engine.
    Engine(EngineFormat),
    /// The kernel version this build's workers run. A started process's
    /// start stamp names the generation it runs as, which includes it:
    /// identity, not a stored integer — see [`FormatProbe::IdentityOnly`].
    KernelVersion,
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
            DurableFormat::SessionCheckpointManifest => "session checkpoint manifest",
            DurableFormat::CheckpointComponentEncoding => "checkpoint component encoding",
            DurableFormat::SessionHeadMeta => "session head meta",
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
            DurableFormat::KernelDocument => "kernel document",
            DurableFormat::KernelParkedState => "kernel parked state",
            DurableFormat::KernelSavedFunction => "kernel saved function",
            DurableFormat::RlmSnapshotEnvelope => "RLM snapshot envelope",
            DurableFormat::RlmDriverState => "RLM driver state",
            DurableFormat::Engine(format) => format.name,
            DurableFormat::KernelVersion => "kernel version",
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
            DurableFormat::SessionCheckpointManifest => UpgradePolicy::Migrate,
            DurableFormat::CheckpointComponentEncoding => UpgradePolicy::Migrate,
            DurableFormat::SessionHeadMeta => UpgradePolicy::Migrate,
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
            DurableFormat::KernelDocument => UpgradePolicy::Migrate,
            DurableFormat::KernelParkedState => UpgradePolicy::Migrate,
            DurableFormat::KernelSavedFunction => UpgradePolicy::Migrate,
            DurableFormat::RlmSnapshotEnvelope => UpgradePolicy::Migrate,
            DurableFormat::RlmDriverState => UpgradePolicy::Migrate,
            DurableFormat::Engine(format) => format.upgrade_policy,
            DurableFormat::KernelVersion => UpgradePolicy::Migrate,
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
            format: DurableFormat::KernelDocument,
            version: FormatVersion::Counter(KERNEL_DOCUMENT_SCHEMA_VERSION),
            owning_crate: "lash-vm-runtime",
            constant: "KERNEL_DOCUMENT_SCHEMA_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::KernelParkedState,
            version: FormatVersion::Counter(KERNEL_PARKED_STATE_VERSION),
            owning_crate: "lash-vm-runtime",
            constant: "KERNEL_PARKED_STATE_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::KernelSavedFunction,
            version: FormatVersion::Counter(KERNEL_SAVED_FUNCTION_VERSION),
            owning_crate: "lash-vm-runtime",
            constant: "KERNEL_SAVED_FUNCTION_VERSION",
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
            format: DurableFormat::RlmDriverState,
            version: FormatVersion::Counter(RLM_DRIVER_STATE_VERSION),
            owning_crate: "lash-protocol-rlm",
            constant: "RLM_DRIVER_STATE_VERSION",
            probe: FormatProbe::Comparable,
        },
        #[cfg(feature = "rlm")]
        DurableFormatEntry {
            format: DurableFormat::KernelVersion,
            version: FormatVersion::Counter(LASH_KERNEL_VERSION),
            owning_crate: "lash-vm-runtime",
            constant: "LASH_KERNEL_VERSION",
            probe: FormatProbe::IdentityOnly,
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
/// `None` means the format is not part of this build — the Lash VM and RLM
/// rows are absent without the `rlm` feature — which is a different answer from
/// "version zero" and is reported as such.
pub fn durable_format(format: DurableFormat) -> Option<DurableFormatEntry> {
    durable_formats().find(|entry| entry.format == format)
}

/// The durable formats actor state holds beyond the runtime core's own
/// (ADR 0106 §1): with `rlm`, the parked kernel run a code cell or a
/// process body resumes from, and the RLM snapshot
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
            FormatSurface::new("kernel-parked-state", KERNEL_PARKED_STATE_VERSION),
            FormatSurface::new("kernel-saved-function", KERNEL_SAVED_FUNCTION_VERSION),
            FormatSurface::new("rlm-snapshot", RLM_SNAPSHOT_VERSION),
        ]
    }
    #[cfg(not(feature = "rlm"))]
    {
        Vec::new()
    }
}

/// [`actor_state_surfaces`] as the previous build declared them, for the
/// formats this build carries forward from it (ADR 0106 §1): with `rlm`,
/// when this build also interprets the kernel version before its own, the
/// parked kernel run of that version. A process the previous build parked
/// is stamped with the set these make, and a node of this build claims it
/// to migrate it. Empty when this build interprets one kernel version.
pub fn previous_actor_state_surfaces() -> Vec<lash_core::durable_port::FormatSurface> {
    #[cfg(feature = "rlm")]
    {
        lash_vm_runtime::previous_kernel_version()
            .map(kernel_actor_state_surfaces)
            .unwrap_or_default()
    }
    #[cfg(not(feature = "rlm"))]
    {
        Vec::new()
    }
}

/// [`actor_state_surfaces`] as a build that parks kernel runs under kernel
/// version `kernel` declares them.
#[cfg(feature = "rlm")]
pub fn kernel_actor_state_surfaces(kernel: u32) -> Vec<lash_core::durable_port::FormatSurface> {
    use lash_core::durable_port::FormatSurface;
    vec![
        FormatSurface::new("kernel-parked-state", kernel),
        FormatSurface::new("kernel-saved-function", KERNEL_SAVED_FUNCTION_VERSION),
        FormatSurface::new("rlm-snapshot", RLM_SNAPSHOT_VERSION),
    ]
}
