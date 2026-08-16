use thiserror::Error;

// v11 carries Lashlang snapshot v5, whose stricter heap reference wire shape
// changes embedded global bytes and therefore their component identities.
// v10 added serializable lashlang call frames and closure heap objects. v9 was
// one shape carrying two changes that each claimed v8 independently:
// the inline-versus-leaf size line applies to globals and files alike, and a
// persisted value body is the canonical Lashlang envelope, which now carries
// heap meters. Neither v8 is decodable — a store written by either one drains
// or is recreated, like every version boundary before it.
pub(super) const RLM_SNAPSHOT_VERSION: u32 = 11;

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
    #[error("RLM snapshot engine `{found}` is unsupported; expected `{expected}`")]
    EngineMismatch { expected: String, found: String },
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
