use thiserror::Error;

/// Version of the durable RLM snapshot envelope stored behind a session
/// checkpoint component.
///
/// Re-exported by the facade's `formats` manifest so a host can read it before
/// wiring a store; the history below is why each boundary is a version rather
/// than a decode failure.
///
// v14 removes the obsolete guest scratch-file section. Older snapshots fail
// closed with the standard drain-or-recreate remedy.
// v13 carries Lashlang snapshot v7 and VM continuation v8: a heap error's brand
// serializes by name, and the two substrate-minted brands are names an older
// reader cannot decode, so the boundary has to be a version and not a decode
// failure.
// v12 carried Lashlang snapshot v6 and its durable RegExpMatch heap kind.
// v11 carried Lashlang snapshot v5, whose stricter heap reference wire shape
// changes embedded global bytes and therefore their component identities.
// v10 added serializable lashlang call frames and closure heap objects. v9 was
// one shape carrying two changes that each claimed v8 independently:
// the inline-versus-leaf size line applies to globals and files alike, and a
// persisted value body is the canonical Lashlang envelope, which now carries
// heap meters. Neither v8 is decodable — a store written by either one drains
// or is recreated, like every version boundary before it.
pub const RLM_SNAPSHOT_VERSION: u32 = 14;

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
    #[error("RLM canonical Lashlang snapshot is invalid: {0}")]
    Lashlang(#[from] lashlang::SnapshotDecodeError),
}
