use thiserror::Error;

// v8 applies the inline-versus-leaf size line to globals and files alike.
// Older snapshots are rejected, never compatibility-decoded.
pub(super) const RLM_SNAPSHOT_VERSION: u32 = 8;

const CUTOVER_REMEDY: &str = "drain in-flight sessions on the old build before deploying this build, or recreate development/test stores";

#[derive(Debug, Error)]
pub(crate) enum RlmSnapshotError {
    #[error("RLM snapshot envelope exceeds the maximum MessagePack nesting depth of {limit}")]
    EnvelopeDepthLimitExceeded { limit: usize },
    #[error("non-canonical RLM snapshot envelope at `{location}`: {reason}")]
    NonCanonicalEnvelope { location: String, reason: String },
    #[error(
        "RLM snapshot format is incompatible with canonical typed MessagePack: {details}; {CUTOVER_REMEDY}"
    )]
    FormatMismatch { details: String },
    #[error(
        "RLM snapshot version {found} is incompatible with version {expected}; {CUTOVER_REMEDY}"
    )]
    VersionMismatch { expected: u32, found: u32 },
    #[error("RLM snapshot engine `{found}` is unsupported; expected `lashlang`")]
    EngineMismatch { found: String },
    #[error(
        "RLM snapshot logical key `{logical_key}` references missing leaf component `{component}`"
    )]
    MissingLeaf {
        logical_key: String,
        component: String,
    },
    #[error(
        "RLM snapshot logical key `{logical_key}` references leaf component `{component}` whose content address is `{actual_component}`"
    )]
    LeafHashMismatch {
        logical_key: String,
        component: String,
        actual_component: String,
    },
    #[error(
        "RLM snapshot root/leaf set is inconsistent; missing={missing:?}, unexpected={unexpected:?}"
    )]
    LeafSetMismatch {
        missing: Vec<String>,
        unexpected: Vec<String>,
    },
    #[error("RLM scratch-file snapshot is invalid: {0}")]
    Scratch(#[from] super::files::ScratchFileError),
    #[error("RLM canonical Lashlang snapshot is invalid: {0}")]
    Lashlang(#[from] lashlang::SnapshotDecodeError),
}
