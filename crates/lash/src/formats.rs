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
//! [`FormatVersion`] has three shapes rather than one. A format whose boundary
//! is a forward-only fence rather than an exact match belongs here with that
//! contract stated; leaving it out because the enum had no shape for it would
//! have made the manifest quietly narrower than the boundary it describes.
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
//! - **Store schema versions.** SQLite's four `user_version` stamps and
//!   PostgreSQL's component stamp belong to their backends, version on their
//!   own cadence, and are read from the deployment rather than from the build.
//!   They come back from a backend's
//!   [`StorePreflight`](crate::persistence::StorePreflight) instead.
//! - **Wire protocol versions.** `REMOTE_PROTOCOL_VERSION` and the trace
//!   schema version gate a live peer or a reader, not parked durable bytes.
//!
//! # Feature gating is honest, not incidental
//!
//! The Lashlang VM and RLM formats exist only when the `rlm` feature is on,
//! because the crates that define them are optional dependencies. A build
//! without `rlm` writes none of those formats, so [`durable_formats`] does not
//! list them. Module artifacts are different: the artifact store is a
//! non-optional durable dependency, and its identity-verified JSON is listed in
//! every build.

pub use lash_core::store::{
    CHECKPOINT_COMPONENT_ENCODING_VERSION, SESSION_CHECKPOINT_SCHEMA_VERSION,
    SESSION_HEAD_META_SCHEMA_VERSION,
};
pub use lash_core::{PROCESS_WAKE_DELIVERY_FORMAT_VERSION, SESSION_NODE_BODY_SCHEMA_VERSION};
#[cfg(feature = "rlm")]
pub use lash_lashlang_runtime::LASHLANG_SEGMENT_STATE_VERSION;
#[cfg(feature = "rlm")]
pub use lash_protocol_rlm::RLM_SNAPSHOT_VERSION;
pub use lashlang::LASHLANG_SEMANTIC_HASH_VERSION;
#[cfg(feature = "rlm")]
pub use lashlang::{
    BYTECODE_FORMAT_VERSION, HEAP_SIZE_SCHEDULE_VERSION, LASHLANG_SNAPSHOT_VERSION,
    LASHLANG_VM_ABI_VERSION, VM_CONTINUATION_FORMAT_VERSION,
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
    ///
    /// The one forward-only format in this table — see
    /// [`FormatVersion::ForwardOnly`].
    SessionNodeBody,
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
            DurableFormat::Bytecode => "bytecode",
            DurableFormat::VmContinuation => "VM continuation",
            DurableFormat::LashlangSnapshot => "Lashlang snapshot",
            DurableFormat::HeapSizeSchedule => "heap size schedule",
            DurableFormat::LashlangSegmentHandover => "Lashlang segment handover",
            DurableFormat::RlmSnapshotEnvelope => "RLM snapshot envelope",
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
    /// A forward-only generation fence: stored bytes at this generation or
    /// *older* load, and a strictly newer generation is refused.
    ///
    /// The comparison is one-directional, which is a different contract from
    /// [`FormatVersion::Counter`] and not a weaker version of it. Immutable
    /// history is why: a session-graph node is never rewritten, so a body from
    /// an older generation still means exactly what it meant, while a body from
    /// a generation this build has never seen cannot be given a shape. Reporting
    /// such a fence as a counter would tell an operator that rolling *back* is a
    /// refusal when it is the supported direction.
    ForwardOnly(u32),
}

impl std::fmt::Display for FormatVersion {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FormatVersion::Counter(version) => write!(f, "{version}"),
            FormatVersion::Identity(identity) => write!(f, "{identity}"),
            // The suffix is the contract, not decoration: the bare integer
            // would read as an exact-match boundary.
            FormatVersion::ForwardOnly(generation) => write!(f, "{generation} or older"),
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
/// language substrate, then the protocol envelope above it, so a rendered
/// report reads outward from the store.
pub fn durable_formats() -> &'static [DurableFormatEntry] {
    &[
        DurableFormatEntry {
            format: DurableFormat::ModuleArtifact,
            version: FormatVersion::Identity(LASHLANG_SEMANTIC_HASH_VERSION),
            owning_crate: "lashlang",
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
            version: FormatVersion::ForwardOnly(SESSION_NODE_BODY_SCHEMA_VERSION),
            owning_crate: "lash-core",
            constant: "SESSION_NODE_BODY_SCHEMA_VERSION",
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
            format: DurableFormat::VmAbi,
            version: FormatVersion::Identity(LASHLANG_VM_ABI_VERSION),
            owning_crate: "lashlang",
            constant: "LASHLANG_VM_ABI_VERSION",
            probe: FormatProbe::NotPersisted,
        },
    ]
}

/// The manifest row for one format, when this build carries it.
///
/// `None` means the format is not part of this build — the Lashlang and RLM
/// rows are absent without the `rlm` feature — which is a different answer from
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
    fn a_version_renders_as_the_operator_would_compare_it() {
        assert_eq!(FormatVersion::Counter(8).to_string(), "8");
        assert_eq!(
            FormatVersion::Identity("lashlang-vm-abi-v6").to_string(),
            "lashlang-vm-abi-v6"
        );
        // A forward-only fence must not render as a bare integer: the number
        // alone reads as an exact-match boundary, which is the wrong contract.
        assert_eq!(FormatVersion::ForwardOnly(1).to_string(), "1 or older");
    }

    #[test]
    fn the_forward_only_fence_is_listed_with_its_own_comparison_shape() {
        // Omitting it because the enum had no shape for a one-directional
        // boundary is exactly how a manifest becomes narrower than the truth.
        let entry = durable_format(DurableFormat::SessionNodeBody)
            .expect("graph node bodies are in every build");
        assert_eq!(
            entry.version,
            FormatVersion::ForwardOnly(lash_core::SESSION_NODE_BODY_SCHEMA_VERSION)
        );
        assert_eq!(entry.owning_crate, "lash-core");
        assert!(
            !matches!(entry.version, FormatVersion::Counter(_)),
            "a forward-only fence reported as a counter tells an operator that \
             rolling back is a refusal when it is the supported direction"
        );
    }
}
