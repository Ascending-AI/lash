//! Node capabilities and claim diagnostics. Package content uses immutable records.

use crate::formats::FormatSet;
use crate::{DurableError, StoreFailure, StoreFailureKind};
pub use lash_vocabulary::claims::{ClaimRequirements, NodeCapabilities};

/// One live node, as the fleet gates read it (P1: one entry per node).
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct NodeCapability {
    /// The format sets the node decodes.
    pub decodes: Vec<FormatSet>,
    /// Machine and registered execution capabilities offered by the node.
    pub capabilities: NodeCapabilities,
}

fn corrupt(message: String) -> DurableError {
    DurableError::Store(StoreFailure {
        kind: StoreFailureKind::Corrupt,
        message,
    })
}

/// The stored spelling of a node's capabilities: canonical JSON, each set
/// sorted.
#[must_use]
pub fn encode_capabilities(capabilities: &NodeCapabilities) -> String {
    serde_json::to_string(capabilities).unwrap_or_else(|_| unreachable!("string keys, no floats"))
}

fn decode_capabilities(text: &str, node: &str) -> Result<NodeCapabilities, DurableError> {
    serde_json::from_str(text).map_err(|_| corrupt(format!("node {node}'s capabilities `{text}`")))
}

/// One row of the live-node listing: `(node, formats_json,
/// capabilities_json)`.
pub type NodeRow = (String, String, String);

/// The live nodes the listing rows spell, one row per node. Both backends'
/// reads.
///
/// # Errors
///
/// A row that does not decode: the store is corrupt.
pub fn nodes_of(rows: Vec<NodeRow>) -> Result<Vec<NodeCapability>, DurableError> {
    rows.into_iter()
        .map(|(node, formats, capabilities)| {
            let sets: Vec<String> = serde_json::from_str(&formats)
                .map_err(|_| corrupt(format!("node {node}'s format sets `{formats}`")))?;
            Ok(NodeCapability {
                decodes: sets.into_iter().map(FormatSet::new).collect(),
                capabilities: decode_capabilities(&capabilities, &node)?,
            })
        })
        .collect()
}

/// The actors and stored requirements the strand read's rows spell, each
/// `(actor_key, requires)`. Both backends' reads.
///
/// # Errors
///
/// A row that does not decode: the store is corrupt.
pub fn requirement_rows(
    rows: Vec<(String, String)>,
) -> Result<Vec<(crate::ActorKey, Vec<String>)>, DurableError> {
    rows.into_iter()
        .map(|(actor, requires)| {
            let key = crate::ActorKey::parse(&actor)
                .map_err(|error| corrupt(format!("actor key `{actor}`: {error}")))?;
            let requires = crate::port::parse_requires(Some(&requires))
                .map_err(|error| corrupt(format!("actor {actor}'s requirements: {error}")))?;
            Ok((key, requires))
        })
        .collect()
}

/// What strands work whose claim requirements are `requires` while `live`
/// are the live nodes (FIG-6993): `None` when some live node offers every
/// requirement it names, else what the live node closest to running
/// it lacks (fewest unmet), or the whole record when no node is live.
/// All six source requirement families participate.
#[must_use]
pub fn strand(requires: &[String], live: &[NodeCapability]) -> Option<ClaimRequirements> {
    let record = lash_core_store::obligations::ObligationToken::requirements_of(
        requires.iter().map(String::as_str),
    );
    if record.tokens().next().is_none() {
        return None;
    }
    let size = |unmet: &ClaimRequirements| unmet.tokens().count();
    let closest = live
        .iter()
        .map(|node| record.unmet_by(&node.capabilities))
        .min_by_key(size);
    match closest {
        Some(unmet) if unmet.tokens().next().is_none() => None,
        Some(unmet) => Some(unmet),
        None => Some(record),
    }
}

pub use lash_core_store::packages::PackageRefusal;

use lash_core_store::artifact_referrer::{ArtifactName, ReferrerClaim, ResolvedArtifactCleanup};
pub use lash_core_store::packages::{LinkRecord, PackageRecord};
pub use lash_vocabulary as vocabulary;
use lash_vocabulary::package::{LinkId, PackageId, PublicationKey, PublicationRecord};

/// Trusted package content storage. A successful hold retains the whole DAG
/// under the existing module-artifact referrer, never a permanent catalog.
#[async_trait::async_trait]
pub trait PackageStore: Send + Sync {
    async fn put_link(
        &self,
        claim: &ReferrerClaim,
        record: &LinkRecord,
        artifacts: &[ArtifactName],
    ) -> Result<(), DurableError>;
    async fn put_package(
        &self,
        claim: &ReferrerClaim,
        record: &PackageRecord,
    ) -> Result<(), DurableError>;
    /// Append an independent JSON label sidecar. The claim must already hold
    /// the Package; labels neither change its identity nor create roots.
    async fn put_labels(
        &self,
        claim: &ReferrerClaim,
        id: &PackageId,
        json: &str,
    ) -> Result<(), DurableError>;
    /// Distinct literal sidecars in digest order, independent of publication order.
    async fn labels(&self, id: &PackageId) -> Result<Vec<String>, DurableError>;
    async fn publish(
        &self,
        claim: &ReferrerClaim,
        record: &PublicationRecord,
    ) -> Result<PublicationKey, DurableError>;
    async fn link(&self, id: &LinkId) -> Result<Option<LinkRecord>, DurableError>;
    async fn package(&self, id: &PackageId) -> Result<Option<PackageRecord>, DurableError>;
    async fn publication(
        &self,
        key: &PublicationKey,
    ) -> Result<Option<PublicationRecord>, DurableError>;
    async fn acquire_publication(
        &self,
        claim: &ReferrerClaim,
        key: &PublicationKey,
    ) -> Result<(), DurableError>;
    async fn end_package_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), DurableError>;
}

/// Backend primitive: every dependency must still exist when the root and
/// complete closure are acquired in one transaction under the referrer fence.
#[async_trait::async_trait]
pub trait PackageStorage: Send + Sync {
    async fn package_referrer_ended(
        &self,
        referrer: &lash_core_store::artifact_referrer::ArtifactReferrer,
    ) -> Result<bool, DurableError>;
    async fn hold_package_record(
        &self,
        claim: &ReferrerClaim,
        reference: &str,
        bytes: Option<&[u8]>,
        dependencies: &[String],
    ) -> Result<(), DurableError>;
    async fn put_package_labels(
        &self,
        claim: &ReferrerClaim,
        reference: &str,
        digest: &[u8; 32],
        json: &str,
    ) -> Result<(), DurableError>;
    async fn package_labels(&self, reference: &str) -> Result<Vec<String>, DurableError>;
    async fn load_package_record(&self, reference: &str) -> Result<Option<Vec<u8>>, DurableError>;
    async fn end_package_records(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), DurableError>;
}

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredLink {
    record: LinkRecord,
    artifacts: std::collections::BTreeSet<ArtifactName>,
}

#[must_use]
pub fn link_reference(id: &LinkId) -> String {
    format!("link:v1:{id}")
}
#[must_use]
pub fn package_reference(id: &PackageId) -> String {
    format!("package:v1:{id}")
}
#[must_use]
pub fn publication_reference(key: &PublicationKey) -> String {
    let bytes = lash_vocabulary::tokens::encode_body(key);
    format!("publication:v1:{}", blake3::hash(&bytes).to_hex())
}

fn encode<T: serde::Serialize>(record: &T) -> Result<Vec<u8>, DurableError> {
    serde_json::to_vec(record).map_err(|error| corrupt(format!("package record: {error}")))
}
fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, DurableError> {
    serde_json::from_slice(bytes).map_err(|error| corrupt(format!("package record: {error}")))
}
fn module_reference(artifact: &ArtifactName) -> Result<String, DurableError> {
    if artifact.store != lash_core_store::artifact_referrer::ArtifactStoreId::KernelDocument {
        return Err(PackageRefusal::Identity(artifact.artifact_ref.clone()).into());
    }
    Ok(artifact.artifact_ref.clone())
}

/// Read immutable records outside the writer, then reacquire all of their
/// pointers in one transaction. A raced last-referrer cleanup yields Missing.
async fn closure<S: PackageStorage + ?Sized>(
    store: &S,
    roots: Vec<String>,
) -> Result<std::collections::BTreeSet<String>, DurableError> {
    use lash_vocabulary::package::PublishedContent;
    use lash_vocabulary::package::{Fill, MemberKind};
    let mut found = std::collections::BTreeSet::new();
    let mut active = std::collections::BTreeSet::new();
    let mut pending: Vec<(String, bool)> = roots
        .into_iter()
        .map(|reference| (reference, false))
        .collect();
    while let Some((reference, leaving)) = pending.pop() {
        if leaving {
            active.remove(&reference);
            found.insert(reference);
            continue;
        }
        if found.contains(&reference) {
            continue;
        }
        if !active.insert(reference.clone()) {
            return Err(PackageRefusal::Cycle.into());
        }
        let bytes = store
            .load_package_record(&reference)
            .await?
            .ok_or_else(|| PackageRefusal::Missing(reference.clone()))?;
        let mut children = Vec::new();
        if let Some(id) = reference.strip_prefix("link:v1:") {
            let stored: StoredLink = decode(&bytes)?;
            if stored.record.id.to_string() != id
                || stored.record.link.identity() != stored.record.id
            {
                return Err(PackageRefusal::Identity(reference).into());
            }
            children.push(stored.record.link.document.to_string());
            children.extend(
                stored
                    .artifacts
                    .iter()
                    .map(module_reference)
                    .collect::<Result<Vec<_>, _>>()?,
            );
            children.extend(
                stored
                    .record
                    .link
                    .fills
                    .values()
                    .filter_map(|fill| match fill {
                        Fill::Export { link, .. } => Some(link_reference(link)),
                        _ => None,
                    }),
            );
        } else if let Some(id) = reference.strip_prefix("package:v1:") {
            let record: PackageRecord = decode(&bytes)?;
            if record.id.to_string() != id || record.package.identity() != record.id {
                return Err(PackageRefusal::Identity(reference).into());
            }
            children.extend(record.package.members.values().filter_map(
                |member| match &member.kind {
                    MemberKind::Export { link, .. } => Some(link_reference(link)),
                    _ => None,
                },
            ));
        } else if reference.starts_with("publication:v1:") {
            let record: PublicationRecord = decode(&bytes)?;
            if publication_reference(&record.key()) != reference
                || record.path.as_slice().first() != Some(&record.context.root)
            {
                return Err(PackageRefusal::Identity(reference).into());
            }
            if let PublishedContent::Package(id) = record.content {
                children.push(package_reference(&id));
            }
            children.extend(record.code_links.iter().map(link_reference));
            children.extend(
                record
                    .imported_publications
                    .iter()
                    .map(publication_reference),
            );
        }
        pending.push((reference, true));
        pending.extend(children.into_iter().map(|child| (child, false)));
    }
    Ok(found)
}

#[async_trait::async_trait]
impl<S: PackageStorage + ?Sized> PackageStore for S {
    async fn put_link(
        &self,
        claim: &ReferrerClaim,
        record: &LinkRecord,
        artifacts: &[ArtifactName],
    ) -> Result<(), DurableError> {
        if record.link.identity() != record.id {
            return Err(PackageRefusal::Identity(link_reference(&record.id)).into());
        }
        let stored = StoredLink {
            record: record.clone(),
            artifacts: artifacts.iter().cloned().collect(),
        };
        let mut roots = vec![record.link.document.to_string()];
        roots.extend(
            artifacts
                .iter()
                .map(module_reference)
                .collect::<Result<Vec<_>, _>>()?,
        );
        roots.extend(record.link.fills.values().filter_map(|fill| match fill {
            lash_vocabulary::package::Fill::Export { link, .. } => Some(link_reference(link)),
            _ => None,
        }));
        let dependencies: Vec<_> = closure(self, roots).await?.into_iter().collect();
        self.hold_package_record(
            claim,
            &link_reference(&record.id),
            Some(&encode(&stored)?),
            &dependencies,
        )
        .await
    }
    async fn put_package(
        &self,
        claim: &ReferrerClaim,
        record: &PackageRecord,
    ) -> Result<(), DurableError> {
        if record.package.identity() != record.id {
            return Err(PackageRefusal::Identity(package_reference(&record.id)).into());
        }
        let roots = record
            .package
            .members
            .values()
            .filter_map(|member| match &member.kind {
                lash_vocabulary::package::MemberKind::Export { link, .. } => {
                    Some(link_reference(link))
                }
                _ => None,
            })
            .collect();
        let dependencies: Vec<_> = closure(self, roots).await?.into_iter().collect();
        self.hold_package_record(
            claim,
            &package_reference(&record.id),
            Some(&encode(record)?),
            &dependencies,
        )
        .await
    }
    async fn put_labels(
        &self,
        claim: &ReferrerClaim,
        id: &PackageId,
        json: &str,
    ) -> Result<(), DurableError> {
        serde_json::from_str::<serde_json::Value>(json)
            .map_err(|error| PackageRefusal::Identity(format!("package labels: {error}")))?;
        let digest = blake3::hash(json.as_bytes());
        self.put_package_labels(claim, &package_reference(id), digest.as_bytes(), json)
            .await
    }
    async fn labels(&self, id: &PackageId) -> Result<Vec<String>, DurableError> {
        self.package_labels(&package_reference(id)).await
    }
    async fn publish(
        &self,
        claim: &ReferrerClaim,
        record: &PublicationRecord,
    ) -> Result<PublicationKey, DurableError> {
        use lash_vocabulary::package::PublishedContent;
        if record.path.as_slice().first() != Some(&record.context.root) {
            return Err(PackageRefusal::Publication.into());
        }
        let mut roots: Vec<_> = record.code_links.iter().map(link_reference).collect();
        match &record.content {
            PublishedContent::Package(id) => {
                let package = self
                    .package(id)
                    .await?
                    .ok_or_else(|| PackageRefusal::Missing(package_reference(id)))?;
                let links: std::collections::BTreeSet<_> = package
                    .package
                    .members
                    .values()
                    .filter_map(|member| match &member.kind {
                        lash_vocabulary::package::MemberKind::Export { link, .. } => Some(*link),
                        _ => None,
                    })
                    .collect();
                if package.package.path != record.path || links != record.code_links {
                    return Err(PackageRefusal::Publication.into());
                }
                roots.push(package_reference(id));
            }
            PublishedContent::Deferred(package) => {
                if package.path != record.path || !record.code_links.is_empty() {
                    return Err(PackageRefusal::Publication.into());
                }
            }
        }
        roots.extend(
            record
                .imported_publications
                .iter()
                .map(publication_reference),
        );
        let dependencies: Vec<_> = closure(self, roots).await?.into_iter().collect();
        let key = record.key();
        self.hold_package_record(
            claim,
            &publication_reference(&key),
            Some(&encode(record)?),
            &dependencies,
        )
        .await?;
        Ok(key)
    }
    async fn link(&self, id: &LinkId) -> Result<Option<LinkRecord>, DurableError> {
        let Some(bytes) = self.load_package_record(&link_reference(id)).await? else {
            return Ok(None);
        };
        let stored: StoredLink = decode(&bytes)?;
        if stored.record.id != *id || stored.record.link.identity() != *id {
            return Err(PackageRefusal::Identity(link_reference(id)).into());
        }
        Ok(Some(stored.record))
    }
    async fn package(&self, id: &PackageId) -> Result<Option<PackageRecord>, DurableError> {
        let Some(bytes) = self.load_package_record(&package_reference(id)).await? else {
            return Ok(None);
        };
        let record: PackageRecord = decode(&bytes)?;
        if record.id != *id || record.package.identity() != *id {
            return Err(PackageRefusal::Identity(package_reference(id)).into());
        }
        Ok(Some(record))
    }
    async fn publication(
        &self,
        key: &PublicationKey,
    ) -> Result<Option<PublicationRecord>, DurableError> {
        let Some(bytes) = self
            .load_package_record(&publication_reference(key))
            .await?
        else {
            return Ok(None);
        };
        let record: PublicationRecord = decode(&bytes)?;
        if record.key() != *key || record.path.as_slice().first() != Some(&record.context.root) {
            return Err(PackageRefusal::Identity(publication_reference(key)).into());
        }
        Ok(Some(record))
    }
    async fn acquire_publication(
        &self,
        claim: &ReferrerClaim,
        key: &PublicationKey,
    ) -> Result<(), DurableError> {
        let reference = publication_reference(key);
        let dependencies: Vec<_> = closure(self, vec![reference.clone()])
            .await?
            .into_iter()
            .collect();
        self.hold_package_record(claim, &reference, None, &dependencies)
            .await
    }
    async fn end_package_referrer(
        &self,
        cleanup: &ResolvedArtifactCleanup,
    ) -> Result<(), DurableError> {
        let expanded = expand_package_cleanup(self, cleanup).await?;
        self.end_package_records(&expanded).await
    }
}

/// Expand a carry of a package root into exact edges for its complete DAG.
/// Generic module cleanup calls this too: a saved callable's Link must not
/// survive without its defining Document and referenced Links.
pub async fn expand_package_cleanup<S: PackageStorage + ?Sized>(
    store: &S,
    cleanup: &ResolvedArtifactCleanup,
) -> Result<ResolvedArtifactCleanup, DurableError> {
    if cleanup.carries.is_empty() {
        return Ok(cleanup.clone());
    }
    if store.package_referrer_ended(&cleanup.referrer).await? {
        return Ok(ResolvedArtifactCleanup {
            referrer: cleanup.referrer.clone(),
            carries: Vec::new(),
        });
    }
    let mut expanded = cleanup.clone();
    for carry in &cleanup.carries {
        let reference = module_reference(&carry.artifact)?;
        if !["link:v1:", "package:v1:", "publication:v1:"]
            .iter()
            .any(|prefix| reference.starts_with(prefix))
        {
            continue;
        }
        for artifact_ref in closure(store, vec![reference]).await? {
            let carry = lash_core_store::artifact_referrer::ArtifactCarry {
                artifact: ArtifactName {
                    store: carry.artifact.store.clone(),
                    artifact_ref,
                },
                to: carry.to.clone(),
            };
            if !expanded.carries.contains(&carry) {
                expanded.carries.push(carry);
            }
        }
    }
    Ok(expanded)
}
