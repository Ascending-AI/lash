//! Durable environment lifetimes and their graph children.
//!
//! Publication: actor fence -> session history -> sorted version owners ->
//! graph blobs -> head/run roots -> retirement holds.
//! Package acquisition: actor/history (when applicable) -> version owner ->
//! sorted referrers -> sorted artifacts. Provider work runs outside locks.
//! Retirement: advisory key -> version owners (SKIP LOCKED) -> graph blobs
//! (SKIP LOCKED) -> enqueue only. The relay later takes referrer/artifact locks.
//! A peel never waits on an owner or an artifact. Permanent Retired rows prevent
//! resurrection even after cleanup and graph bytes have disappeared.
//!
//! Round and head roots store JSON text beside their version anchor. Their
//! graph identity promises the exact stored string, including whitespace and
//! member order, not a semantic one-spelling identity for the typed environment.

use crate::{ActorKey, DurableError, Epoch, StoreFailure, StoreFailureKind};
use lash_core_store::checkpoint_graph::{GraphRefusal, GraphStaging, NodeId, NodeKind, TypedBody};
pub use lash_core_store::environment::*;
use {
    lash_kernel_value::identity::SessionId, lash_kernel_value::tokens::TokenReader,
    lash_kernel_value::tokens::TokenWriter,
};

/// Owner writes join the actor's existing transaction with phase/run writes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum EnvironmentWrite {
    /// Persist the cleanup guard before the caller can hold any package.
    Prepare {
        owner: VersionOwner,
        body_json: String,
    },
    /// Commit with a head or retained run/phase graph reference.
    Publish { owner: VersionOwnerId },
    /// Abandon an unpublished preparation under its original actor epoch.
    Abort { owner: VersionOwnerId },
    /// Compare the whole head, including the observed source frontier.
    Head {
        expected: Option<EnvironmentHead>,
        head: Option<EnvironmentHead>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error, serde::Serialize, serde::Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum EnvironmentRefusal {
    #[error("environment owner {0} is absent")]
    Missing(VersionOwnerId),
    #[error("environment owner {0} is not published")]
    NotPublished(VersionOwnerId),
    #[error("environment owner {0} has permanently retired")]
    Retired(VersionOwnerId),
    #[error("environment owner {0} belongs to another preparation")]
    Preparation(VersionOwnerId),
    #[error("environment head changed")]
    HeadConflict,
    #[error("environment publication has no retained reader")]
    NoReader,
    #[error("environment write requires the session actor")]
    SessionActor,
    #[error("environment content does not match its owner")]
    Content,
}

/// Immutable owner facts and the body that remains available while retained.
#[derive(Clone, Debug, PartialEq)]
pub struct EnvironmentRecord {
    pub owner: VersionOwner,
    pub actor: ActorKey,
    pub preparing_epoch: Epoch,
    pub body: Option<EnvironmentBody>,
}

#[async_trait::async_trait]
pub trait EnvironmentStore: Send + Sync {
    async fn environment_owner(
        &self,
        owner: &VersionOwnerId,
    ) -> Result<Option<EnvironmentRecord>, DurableError>;
    async fn environment_head(
        &self,
        session: &SessionId,
    ) -> Result<Option<EnvironmentHead>, DurableError>;
    /// Atomically retires Preparing only after its actor epoch ended. Published
    /// is never aborted; the graph retirement bridge alone ends it. Missing or
    /// undecodable ownership is a refusal, never permission to reclaim.
    async fn retire_abandoned_environment(
        &self,
        owner: &VersionOwnerId,
    ) -> Result<bool, DurableError>;
}

pub fn corrupt(message: impl std::fmt::Display) -> DurableError {
    DurableError::Store(StoreFailure {
        code: None,
        kind: StoreFailureKind::Corrupt,
        message: format!("environment ownership: {message}"),
    })
}
pub fn json<T: serde::Serialize>(value: &T) -> Result<String, DurableError> {
    serde_json::to_string(value).map_err(corrupt)
}
pub fn decode<T: serde::de::DeserializeOwned>(text: &str) -> Result<T, DurableError> {
    serde_json::from_str(text).map_err(corrupt)
}

/// Small ownership facts for fenced acquisition and graph retention. Reading
/// authority never loads, normalizes or hashes the immutable environment body.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EnvironmentAuthority {
    pub owner: VersionOwner,
    pub actor: ActorKey,
    pub preparing_epoch: Epoch,
}
pub fn authority(
    id: String,
    version: String,
    state: String,
    actor: String,
    epoch: i64,
) -> Result<EnvironmentAuthority, DurableError> {
    if version.len() != 64
        || !version
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
        || epoch < 0
    {
        return Err(corrupt("invalid version identity or preparation epoch"));
    }
    let owner: VersionOwner =
        serde_json::from_value(serde_json::json!({"id":id,"version":version,"state":state}))
            .map_err(corrupt)?;
    let actor = ActorKey::parse(&actor).map_err(corrupt)?;
    if actor.kind() != crate::ActorKind::Session {
        return Err(corrupt("environment owner is not a session actor"));
    }
    Ok(EnvironmentAuthority {
        owner,
        actor,
        preparing_epoch: Epoch(epoch),
    })
}

// Reuse only exact, validated body bytes. Every caller still reads its row
// and checks authority, version and body presence; these entries own no durable
// references. Bound both entry count and retained serialized bytes. Validation
// runs outside the cache lock, then insertion reclassifies under that lock.
struct BodyEntry {
    /// Canonical bytes already belong to `body`; retain another encoding only
    /// when it differs, since cache hits require exact stored bytes.
    json: Option<String>,
    body: EnvironmentBody,
}
impl BodyEntry {
    fn json(&self) -> Option<&str> {
        self.json
            .as_deref()
            .or_else(|| self.body.canonical_json().ok())
    }

    fn bytes(&self) -> usize {
        self.json().map_or(BODY_BYTES, str::len)
    }
}
static BODIES: std::sync::LazyLock<std::sync::Mutex<std::collections::VecDeque<BodyEntry>>> =
    std::sync::LazyLock::new(Default::default);
const BODY_ENTRIES: usize = 32;
const BODY_BYTES: usize = 2 << 20;

/// Decode an exact body, reusing admitted immutable content when resident.
/// The caller retains its validation and live-owner checks on cold input.
pub fn decoded_body(version: &EnvironmentId, json: &str) -> Result<EnvironmentBody, DurableError> {
    use lash_protocol_types::sync::MutexExt;
    if let Some(body) = BODIES
        .lock_recover()
        .iter()
        .find(|entry| entry.body.id() == version && entry.json() == Some(json))
        .map(|entry| entry.body.clone())
    {
        return Ok(body);
    }
    decode(json)
}

fn validated_body(version: &EnvironmentId, json: String) -> Result<EnvironmentBody, DurableError> {
    let body = decoded_body(version, &json)?;
    body.validate().map_err(corrupt)?;
    if body.id() == version {
        remember_body(&body, &json);
    }
    Ok(body)
}

fn remember_body(body: &EnvironmentBody, json: &str) {
    use lash_protocol_types::sync::MutexExt;
    if json.len() > BODY_BYTES {
        return;
    }
    let stored = (body.canonical_json().ok() != Some(json)).then(|| json.to_owned());
    let mut entries = BODIES.lock_recover();
    if entries
        .iter()
        .any(|entry| entry.body.id() == body.id() && entry.json() == Some(json))
    {
        return;
    }
    let mut bytes: usize = entries.iter().map(BodyEntry::bytes).sum();
    while entries.len() >= BODY_ENTRIES || bytes + json.len() > BODY_BYTES {
        if let Some(entry) = entries.pop_front() {
            bytes -= entry.bytes();
        }
    }
    entries.push_back(BodyEntry {
        json: stored,
        body: body.clone(),
    });
}

/// Shared row decoder. Stored state, owner identity and body are all checked.
pub fn record(
    id: String,
    version: String,
    state: String,
    actor: String,
    epoch: i64,
    body: Option<String>,
) -> Result<EnvironmentRecord, DurableError> {
    let EnvironmentAuthority {
        owner,
        actor,
        preparing_epoch,
    } = authority(id, version, state, actor, epoch)?;
    let body = body
        .map(|json| validated_body(owner.version(), json))
        .transpose()?;
    if let Some(body) = &body {
        if body.id() != owner.version() {
            return Err(corrupt("owner/body identity mismatch"));
        }
    } else if owner.state() != VersionOwnerState::Retired {
        return Err(corrupt("live owner has no body"));
    }
    Ok(EnvironmentRecord {
        owner,
        actor,
        preparing_epoch,
        body,
    })
}

pub fn staging() -> GraphStaging {
    GraphStaging::new(lash_core_store::checkpoint_graph::EncodeBudget::new(
        lash_core_store::checkpoint_graph::ExplicitLimit::new(u64::MAX),
    ))
}

/// One graph leaf per owner lifetime, independent of equal content lifetimes.
pub fn stage_version(
    staging: &mut GraphStaging,
    round: &RoundEnvironment,
) -> Result<NodeId, GraphRefusal> {
    let mut out = TokenWriter::new();
    out.seq(2);
    out.text(&round.owner.to_string());
    out.text(&round.version.to_string());
    staging
        .encode(
            NodeKind::ENVIRONMENT_VERSION,
            TypedBody::logical(out.as_bytes()),
        )
        .map_err(GraphRefusal::from)
}

pub fn version_node(round: &RoundEnvironment) -> Result<NodeId, DurableError> {
    stage_version(&mut staging(), round).map_err(Into::into)
}

/// Run/phase codecs embed this node as a typed child, never a JSON-only id.
pub fn stage_round(
    staging: &mut GraphStaging,
    round: &RoundEnvironment,
) -> Result<NodeId, DurableError> {
    let anchor = stage_version(staging, round)?;
    let mut out = TokenWriter::new();
    out.seq(2);
    out.text(&json(round)?);
    out.child(anchor.child_ref());
    Ok(staging
        .encode(
            NodeKind::ROUND_ENVIRONMENT,
            TypedBody::logical(out.as_bytes()),
        )
        .map_err(GraphRefusal::from)?)
}

pub fn stage_head(
    staging: &mut GraphStaging,
    head: &EnvironmentHead,
) -> Result<NodeId, DurableError> {
    let anchor = stage_version(staging, &head.round())?;
    let mut out = TokenWriter::new();
    out.seq(2);
    out.text(&json(head)?);
    out.child(anchor.child_ref());
    Ok(staging
        .encode(
            NodeKind::ENVIRONMENT_HEAD,
            TypedBody::logical(out.as_bytes()),
        )
        .map_err(GraphRefusal::from)?)
}

/// Direct-bodied version leaves cannot hide undecodable ownership behind an
/// absent row. Every installer validates these supplied leaves before locks.
pub fn supplied_versions(
    graph: &crate::domain::graph_commit::CheckedGraph,
) -> Result<Vec<(NodeId, RoundEnvironment)>, DurableError> {
    let mut result = Vec::new();
    for node in &graph.nodes {
        let envelope = lash_core_store::checkpoint_graph::check_envelope(
            &node.bytes,
            node.id,
            &crate::domain::graph::OWNER_READ_LIMITS,
        )
        .map_err(GraphRefusal::from)?;
        if envelope.kind != NodeKind::ENVIRONMENT_VERSION {
            continue;
        }
        let mut input = TokenReader::new(envelope.body);
        input.expect_struct(2).map_err(corrupt)?;
        let owner: VersionOwnerId = serde_json::from_value(serde_json::Value::String(
            input.text().map_err(corrupt)?.to_owned(),
        ))
        .map_err(corrupt)?;
        let version = serde_json::from_value(serde_json::Value::String(
            input.text().map_err(corrupt)?.to_owned(),
        ))
        .map_err(corrupt)?;
        input.finish().map_err(corrupt)?;
        let round = RoundEnvironment { owner, version };
        if version_node(&round)? != node.id {
            return Err(corrupt("noncanonical version leaf"));
        }
        result.push((node.id, round));
    }
    result.sort_by(|a, b| a.1.owner.cmp(&b.1.owner));
    Ok(result)
}

impl EnvironmentWrite {
    /// Validate/encode once, before entering the fenced store transaction.
    pub fn prepare(owner: VersionOwner, body: &EnvironmentBody) -> Result<Self, DurableError> {
        body.validate().map_err(corrupt)?;
        if owner.state() != VersionOwnerState::Preparing || owner.version() != body.id() {
            return Err(EnvironmentRefusal::Content.into());
        }
        let body_json = body.canonical_json().map_err(corrupt)?.to_owned();
        remember_body(body, &body_json);
        Ok(Self::Prepare { owner, body_json })
    }
}

/// Read the recorded reference, including its typed lifetime child. This
/// never substitutes the session's current head or invokes source providers.
pub fn decode_round(
    decoder: &mut lash_core_store::checkpoint_graph::GraphDecoder<'_>,
    id: NodeId,
) -> Result<RoundEnvironment, DurableError> {
    let logical = decoder.logical_body(id, NodeKind::ROUND_ENVIRONMENT)?;
    let mut input = TokenReader::new(&logical);
    input.expect_struct(2).map_err(corrupt)?;
    let round: RoundEnvironment = decode(input.text().map_err(corrupt)?)?;
    let child = NodeId::from(input.child().map_err(corrupt)?);
    input.finish().map_err(corrupt)?;
    if child != version_node(&round)? {
        return Err(EnvironmentRefusal::Content.into());
    }
    decoder.logical_body(child, NodeKind::ENVIRONMENT_VERSION)?;
    Ok(round)
}

/// Decode only the head and its storage nodes, including the typed lifetime
/// anchor. The version's environment body is not part of this graph closure.
pub fn decode_head(
    decoder: &mut lash_core_store::checkpoint_graph::GraphDecoder<'_>,
    id: NodeId,
) -> Result<EnvironmentHead, DurableError> {
    let logical = decoder.logical_body(id, NodeKind::ENVIRONMENT_HEAD)?;
    let mut input = TokenReader::new(&logical);
    input.expect_struct(2).map_err(corrupt)?;
    let head: EnvironmentHead = decode(input.text().map_err(corrupt)?)?;
    let child = NodeId::from(input.child().map_err(corrupt)?);
    input.finish().map_err(corrupt)?;
    if child != version_node(&head.round())? {
        return Err(EnvironmentRefusal::Content.into());
    }
    decoder.logical_body(child, NodeKind::ENVIRONMENT_VERSION)?;
    Ok(head)
}

/// Encoded owner-row mutations, including a preparation's immutable body.
/// The obligation row is part of the same planned mutation budget.
pub(crate) fn measure_write(
    write: &EnvironmentWrite,
    out: &mut TokenWriter,
) -> Result<u64, DurableError> {
    match write {
        EnvironmentWrite::Prepare { owner, body_json } => {
            out.sum(0, 3);
            out.text(&owner.id().to_string());
            out.text(&owner.version().to_string());
            out.text(body_json);
            Ok(2)
        }
        EnvironmentWrite::Publish { owner } => {
            out.sum(1, 1);
            out.text(&owner.to_string());
            Ok(1)
        }
        EnvironmentWrite::Abort { owner } => {
            out.sum(2, 1);
            out.text(&owner.to_string());
            Ok(2)
        }
        EnvironmentWrite::Head { expected, head } => {
            out.sum(3, 2);
            for value in [expected, head] {
                match value {
                    None => out.none(),
                    Some(value) => {
                        out.some();
                        out.text(&json(value)?);
                    }
                }
            }
            Ok(2)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// ENV-BODY-REUSE: identical stored bytes reuse validated immutable
    /// fragments, while every row's ownership and content are still checked.
    #[test]
    fn repeated_body_reads_reuse_content_without_reusing_owner_authority() {
        let body = assemble(
            None,
            [SourceSnapshot {
                source: SourceId("reader-reuse".into()),
                revision: SourceRevision {
                    incarnation: Default::default(),
                    counter: 0,
                },
                fragment: EnvironmentFragment {
                    config: std::collections::BTreeMap::from([(
                        "reader".into(),
                        serde_json::json!({"enabled": true}),
                    )]),
                    ..Default::default()
                },
            }],
        )
        .unwrap();
        let owner = VersionOwner::preparing(body.id().clone());
        let encoded = json(&body).unwrap();
        let read = |version: &EnvironmentId, state: &str, bytes: Option<String>| {
            record(
                owner.id().to_string(),
                version.to_string(),
                state.into(),
                ActorKey::session("body-reader").unwrap().to_string(),
                1,
                bytes,
            )
        };
        let first = read(body.id(), "published", Some(encoded.clone())).unwrap();
        let second = read(body.id(), "published", Some(encoded.clone())).unwrap();
        assert!(
            std::sync::Arc::ptr_eq(
                first
                    .body
                    .as_ref()
                    .unwrap()
                    .fragments()
                    .values()
                    .next()
                    .unwrap(),
                second
                    .body
                    .as_ref()
                    .unwrap()
                    .fragments()
                    .values()
                    .next()
                    .unwrap(),
            ),
            "ENV-BODY-REUSE: identical reads rebuilt validated fragments"
        );
        {
            use lash_protocol_types::sync::MutexExt;
            let entries = BODIES.lock_recover();
            let entry = entries
                .iter()
                .find(|entry| entry.body.id() == body.id() && entry.json() == Some(&encoded))
                .unwrap();
            assert_eq!(
                entry.json().unwrap().as_ptr(),
                first
                    .body
                    .as_ref()
                    .unwrap()
                    .canonical_json()
                    .unwrap()
                    .as_ptr(),
                "ENV-BODY-REUSE: canonical cache bytes duplicate the body's own encoding"
            );
        }
        let spaced = format!(" {encoded} ");
        let spaced_first = read(body.id(), "published", Some(spaced.clone())).unwrap();
        let spaced_second = read(body.id(), "published", Some(spaced)).unwrap();
        let fragment = |record: &EnvironmentRecord| {
            record
                .body
                .as_ref()
                .unwrap()
                .fragments()
                .values()
                .next()
                .unwrap()
                .clone()
        };
        assert!(std::sync::Arc::ptr_eq(
            &fragment(&spaced_first),
            &fragment(&spaced_second)
        ));
        assert!(
            !std::sync::Arc::ptr_eq(&fragment(&first), &fragment(&spaced_first)),
            "ENV-BODY-REUSE: differently encoded input must not reuse a cached validation"
        );
        assert!(read(body.id(), "published", None).is_err());
        let retired = read(body.id(), "retired", None).unwrap();
        assert_eq!(retired.owner.state(), VersionOwnerState::Retired);
        assert!(
            retired.body.is_none(),
            "cached bytes cannot resurrect a retired body"
        );
        let other = assemble(None, []).unwrap();
        assert!(read(other.id(), "published", Some(encoded)).is_err());
        let mut corrupt = serde_json::to_value(&body).unwrap();
        corrupt["id"] = serde_json::to_value(other.id()).unwrap();
        assert!(read(body.id(), "published", Some(corrupt.to_string())).is_err());
    }
}
