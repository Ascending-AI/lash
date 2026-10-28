//! Node capabilities and claim diagnostics. Package content uses immutable records.

use crate::formats::FormatSet;
use crate::{DurableError, StoreFailure, StoreFailureKind};
pub use lash_vocabulary::claims::{ClaimRequirements, NodeCapabilities};

/// One live node, as capability admission reads it (P1: one entry per node).
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
    let record = lash_vocabulary::claims::ObligationToken::requirements_of(
        requires.iter().map(String::as_str),
    );
    record.tokens().next()?;
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
use {
    lash_kernel_value::package::LinkId, lash_kernel_value::package::PackageId,
    lash_vocabulary::package::PublicationKey, lash_vocabulary::package::PublicationRecord,
};

/// One execution admission. Every entry retains its own root and complete
/// provenance under the supplied execution claim, in one store transaction.
/// Documents precede the Links that name them. Package and Publish introduce
/// immutable namespace records; Publication retains an already stored record.
pub enum PackageHold<'a> {
    Document {
        id: lash_kernel_value::names::DocumentId,
        body: &'a [u8],
    },
    Link {
        record: &'a LinkRecord,
        artifacts: &'a [ArtifactName],
    },
    /// Publish a package in the same admission as its publications.
    Package(&'a PackageRecord),
    /// Publish a namespace record, validating supplied or stored content.
    Publish(&'a PublicationRecord),
    Publication(&'a PublicationRecord),
    /// Retain an existing Link and its stored dependency metadata.
    RetainLink(&'a LinkId),
}

mod batch;

/// A prepared immutable root and its closure, passed intact to storage.
#[derive(Debug)]
pub struct PackageRecordHold {
    pub reference: String,
    pub bytes: Option<Vec<u8>>,
    pub document: bool,
    pub dependencies: Vec<String>,
}

/// Prepared publication retention. Each acquisition checks the complete
/// immutable closure and its fresh owner fence atomically.
#[derive(Debug)]
pub struct PreparedPublications {
    publications: Vec<PublicationRecord>,
    holds: Option<Vec<PackageRecordHold>>,
}

/// Trusted package content storage. A successful hold retains the whole DAG
/// under the existing module-artifact referrer, never a permanent catalog.
#[async_trait::async_trait]
pub trait PackageStore: Send + Sync {
    /// Admit documents, Links and accepted publications atomically. A refusal
    /// leaves no root or edge from this batch. Existing dependencies are read
    /// outside the writer and checked again with the claim's fence inside it.
    /// Links supplied by this batch participate in dependency discovery before
    /// publication, so a new Link may import another new Link in the batch.
    async fn acquire_batch(
        &self,
        claim: &ReferrerClaim,
        holds: &[PackageHold<'_>],
    ) -> Result<(), DurableError>;

    /// Prepare retention metadata for immutable publications. A store may defer
    /// discovering their dependency DAG until acquisition.
    async fn prepare_publications(
        &self,
        publications: &[PublicationRecord],
    ) -> Result<PreparedPublications, DurableError> {
        Ok(PreparedPublications {
            publications: publications.to_vec(),
            holds: None,
        })
    }

    /// Acquire a prepared DAG under today's owner, refusing missing pointers.
    async fn acquire_prepared_publications(
        &self,
        claim: &ReferrerClaim,
        prepared: &PreparedPublications,
    ) -> Result<(), DurableError> {
        if prepared.publications.is_empty() {
            return self
                .unheld_publications(claim, &std::collections::BTreeSet::new())
                .await
                .map(|_| ());
        }
        let holds: Vec<_> = prepared
            .publications
            .iter()
            .map(PackageHold::Publication)
            .collect();
        self.acquire_batch(claim, &holds).await
    }

    /// Requested publication closures not held by this exact owner. Checks
    /// owner admission and its permanent fence even for an empty set.
    async fn unheld_publications(
        &self,
        claim: &ReferrerClaim,
        keys: &std::collections::BTreeSet<PublicationKey>,
    ) -> Result<std::collections::BTreeSet<PublicationKey>, DurableError>;

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

/// version_surface = "coexist"
/// version_guard(items(LINK_REF_PREFIX, link_reference, content_dependencies, is_package_reference))
const LINK_REF_PREFIX: &str = "link:v1:";
/// version_surface = "coexist"
/// version_guard(items(PACKAGE_REF_PREFIX, package_reference, content_dependencies, is_package_reference))
const PACKAGE_REF_PREFIX: &str = "package:v1:";
/// version_surface = "coexist"
/// version_guard(items(PUBLICATION_REF_PREFIX, publication_reference, content_dependencies, is_package_reference))
const PUBLICATION_REF_PREFIX: &str = "publication:v1:";

/// Backend primitive: every dependency must still exist when the root and
/// complete closure are acquired in one transaction under the referrer fence.
#[async_trait::async_trait]
pub trait PackageStorage: Send + Sync {
    /// One transaction for every prepared root, its dependency edges and the
    /// claim's cleanup obligation. Missing dependencies or a fenced owner roll
    /// back the complete batch, including documents newly published by it.
    async fn hold_package_batch(
        &self,
        claim: &ReferrerClaim,
        holds: &[PackageRecordHold],
    ) -> Result<(), DurableError>;

    /// Requested publication closures not held by this exact owner. Checks
    /// owner admission and its permanent fence even for an empty set.
    async fn unheld_publication_records(
        &self,
        claim: &ReferrerClaim,
        keys: &std::collections::BTreeSet<PublicationKey>,
    ) -> Result<std::collections::BTreeSet<PublicationKey>, DurableError>;

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
    format!("{LINK_REF_PREFIX}{id}")
}
#[must_use]
pub fn package_reference(id: &PackageId) -> String {
    format!("{PACKAGE_REF_PREFIX}{id}")
}
#[must_use]
pub fn publication_reference(key: &PublicationKey) -> String {
    let bytes = lash_kernel_value::tokens::encode_body(key);
    format!("{PUBLICATION_REF_PREFIX}{}", blake3::hash(&bytes).to_hex())
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

/// Immediate immutable content dependencies. All storage owners use this parser.
///
/// # Errors
/// A record is malformed or does not match its content identity.
pub fn content_dependencies(reference: &str, bytes: &[u8]) -> Result<Vec<String>, DurableError> {
    use {
        lash_kernel_value::package::Fill, lash_kernel_value::package::MemberKind,
        lash_vocabulary::package::PublishedContent,
    };
    let mut children = Vec::new();
    if let Some(id) = reference.strip_prefix(LINK_REF_PREFIX) {
        let stored: StoredLink = decode(bytes)?;
        if stored.record.id.to_string() != id || stored.record.link.identity() != stored.record.id {
            return Err(PackageRefusal::Identity(reference.to_owned()).into());
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
    } else if let Some(id) = reference.strip_prefix(PACKAGE_REF_PREFIX) {
        let record: PackageRecord = decode(bytes)?;
        if record.id.to_string() != id || record.package.identity() != record.id {
            return Err(PackageRefusal::Identity(reference.to_owned()).into());
        }
        children.extend(
            record
                .package
                .members
                .values()
                .filter_map(|member| match &member.kind {
                    MemberKind::Export { link, .. } => Some(link_reference(link)),
                    _ => None,
                }),
        );
    } else if reference.starts_with(PUBLICATION_REF_PREFIX) {
        let record: PublicationRecord = decode(bytes)?;
        if publication_reference(&record.key()) != reference
            || record.path.as_slice().first() != Some(&record.context.root)
        {
            return Err(PackageRefusal::Identity(reference.to_owned()).into());
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
    Ok(children)
}

/// Provenance retained atomically with an execution environment's own bytes.
///
/// # Errors
/// The stored environment does not decode.
pub fn process_env_roots(bytes: &[u8]) -> Result<Vec<String>, DurableError> {
    let env = lash_core_store::process_env::ProcessEnv::from_store_bytes(bytes)
        .map_err(|error| corrupt(format!("process environment: {error}")))?;
    let execution = env.execution();
    Ok(execution
        .publications
        .iter()
        .map(|record| publication_reference(&record.key()))
        .chain(execution.authored.iter().map(link_reference))
        .chain(
            env.packages()
                .publications()
                .iter()
                .map(|record| publication_reference(&record.key())),
        )
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .collect())
}

/// Read immutable records outside the writer, then reacquire all of their
/// pointers in one transaction. A raced last-referrer cleanup yields Missing.
async fn closure<S: PackageStorage + ?Sized>(
    store: &S,
    roots: Vec<String>,
) -> Result<std::collections::BTreeSet<String>, DurableError> {
    closure_with(store, roots, &std::collections::BTreeMap::new()).await
}

async fn closure_with<S: PackageStorage + ?Sized>(
    store: &S,
    roots: Vec<String>,
    prepared: &std::collections::BTreeMap<String, Vec<u8>>,
) -> Result<std::collections::BTreeSet<String>, DurableError> {
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
        // Only package records describe further dependencies. A Document or
        // definition is a terminal artifact: carry its identity to the atomic
        // hold below, which checks presence under the same fence as acquiring
        // the complete closure. Hydrating its body discovers no extra edges.
        if !is_package_reference(&reference) {
            active.remove(&reference);
            found.insert(reference);
            continue;
        }
        let stored;
        let bytes = match prepared.get(&reference) {
            Some(bytes) => bytes,
            None => {
                stored = store
                    .load_package_record(&reference)
                    .await?
                    .ok_or_else(|| PackageRefusal::Missing(reference.clone()))?;
                &stored
            }
        };
        let children = content_dependencies(&reference, bytes)?;
        pending.push((reference, true));
        pending.extend(children.into_iter().map(|child| (child, false)));
    }
    Ok(found)
}

#[async_trait::async_trait]
impl<S: PackageStorage + ?Sized> PackageStore for S {
    async fn acquire_batch(
        &self,
        claim: &ReferrerClaim,
        holds: &[PackageHold<'_>],
    ) -> Result<(), DurableError> {
        batch::acquire(self, claim, holds).await
    }

    async fn prepare_publications(
        &self,
        publications: &[PublicationRecord],
    ) -> Result<PreparedPublications, DurableError> {
        let mut roots = Vec::with_capacity(publications.len());
        for publication in publications {
            let key = publication.key();
            if self.publication(&key).await?.as_ref() != Some(publication) {
                return Err(PackageRefusal::Identity(publication_reference(&key)).into());
            }
            roots.push(publication_reference(&key));
        }
        let dependencies = closure(self, roots).await?;
        let holds = dependencies
            .into_iter()
            .map(|reference| PackageRecordHold {
                reference,
                bytes: None,
                document: false,
                dependencies: Vec::new(),
            })
            .collect();
        Ok(PreparedPublications {
            publications: publications.to_vec(),
            holds: Some(holds),
        })
    }

    async fn acquire_prepared_publications(
        &self,
        claim: &ReferrerClaim,
        prepared: &PreparedPublications,
    ) -> Result<(), DurableError> {
        match &prepared.holds {
            Some(holds) if holds.is_empty() => self
                .unheld_publication_records(claim, &std::collections::BTreeSet::new())
                .await
                .map(|_| ()),
            Some(holds) => self.hold_package_batch(claim, holds).await,
            None => {
                let holds: Vec<_> = prepared
                    .publications
                    .iter()
                    .map(PackageHold::Publication)
                    .collect();
                self.acquire_batch(claim, &holds).await
            }
        }
    }

    async fn unheld_publications(
        &self,
        claim: &ReferrerClaim,
        keys: &std::collections::BTreeSet<PublicationKey>,
    ) -> Result<std::collections::BTreeSet<PublicationKey>, DurableError> {
        self.unheld_publication_records(claim, keys).await
    }

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
            lash_kernel_value::package::Fill::Export { link, .. } => Some(link_reference(link)),
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
        self.acquire_batch(claim, &[PackageHold::Package(record)])
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
        self.acquire_batch(claim, &[PackageHold::Publish(record)])
            .await?;
        Ok(record.key())
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

/// Whether a module-store reference owns immutable package dependencies.
#[must_use]
pub fn is_package_reference(reference: &str) -> bool {
    [LINK_REF_PREFIX, PACKAGE_REF_PREFIX, PUBLICATION_REF_PREFIX]
        .iter()
        .any(|prefix| reference.starts_with(prefix))
}

/// Acquire a content root and its immutable closure under one fenced claim.
///
/// # Errors
/// A dependency was reclaimed, corrupted, or the referrer ended.
pub async fn acquire_content<S: PackageStorage + ?Sized>(
    store: &S,
    claim: &ReferrerClaim,
    reference: &str,
) -> Result<(), DurableError> {
    let dependencies: Vec<_> = closure(store, vec![reference.to_owned()])
        .await?
        .into_iter()
        .collect();
    store
        .hold_package_record(claim, reference, None, &dependencies)
        .await
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
        if !is_package_reference(&reference) {
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

#[cfg(test)]
mod work_laws {
    use super::*;
    use std::collections::BTreeMap;
    use std::sync::Mutex;

    #[derive(Default)]
    struct CountedStorage {
        records: Mutex<BTreeMap<String, Vec<u8>>>,
        reads: Mutex<Vec<String>>,
        holds: Mutex<Vec<String>>,
    }

    #[async_trait::async_trait]
    impl PackageStorage for CountedStorage {
        async fn hold_package_batch(
            &self,
            _: &ReferrerClaim,
            batch: &[PackageRecordHold],
        ) -> Result<(), DurableError> {
            let mut next = self.records.lock().expect("work-law fixture").clone();
            for record in batch {
                if let Some(bytes) = &record.bytes
                    && let Some(previous) = next.insert(record.reference.clone(), bytes.clone())
                    && previous != *bytes
                {
                    return Err(PackageRefusal::Immutable(record.reference.clone()).into());
                }
            }
            for record in batch {
                for reference in std::iter::once(&record.reference).chain(&record.dependencies) {
                    if !next.contains_key(reference) {
                        return Err(PackageRefusal::Missing(reference.clone()).into());
                    }
                }
            }
            *self.records.lock().expect("work-law fixture") = next;
            Ok(())
        }

        async fn unheld_publication_records(
            &self,
            _: &ReferrerClaim,
            _: &std::collections::BTreeSet<PublicationKey>,
        ) -> Result<std::collections::BTreeSet<PublicationKey>, DurableError> {
            unreachable!("content hydration law does not observe publication holds")
        }

        async fn package_referrer_ended(
            &self,
            _: &lash_core_store::artifact_referrer::ArtifactReferrer,
        ) -> Result<bool, DurableError> {
            Ok(false)
        }
        async fn hold_package_record(
            &self,
            _: &ReferrerClaim,
            reference: &str,
            _: Option<&[u8]>,
            dependencies: &[String],
        ) -> Result<(), DurableError> {
            let records = self.records.lock().expect("work-law fixture");
            for dependency in dependencies.iter().map(String::as_str).chain([reference]) {
                if !records.contains_key(dependency) {
                    return Err(PackageRefusal::Missing(dependency.into()).into());
                }
            }
            *self.holds.lock().expect("work-law fixture") = dependencies.to_vec();
            Ok(())
        }
        async fn put_package_labels(
            &self,
            _: &ReferrerClaim,
            _: &str,
            _: &[u8; 32],
            _: &str,
        ) -> Result<(), DurableError> {
            unreachable!("retention does not publish labels")
        }
        async fn package_labels(&self, _: &str) -> Result<Vec<String>, DurableError> {
            unreachable!("retention does not read labels")
        }
        async fn load_package_record(
            &self,
            reference: &str,
        ) -> Result<Option<Vec<u8>>, DurableError> {
            self.reads
                .lock()
                .expect("work-law fixture")
                .push(reference.into());
            Ok(self
                .records
                .lock()
                .expect("work-law fixture")
                .get(reference)
                .cloned())
        }
        async fn end_package_records(
            &self,
            _: &ResolvedArtifactCleanup,
        ) -> Result<(), DurableError> {
            unreachable!("retention does not end a referrer")
        }
    }

    /// FIG-7447: one atomic batch can retain a new Link and the new Link it
    /// imports, in either order. No dependency needs an earlier publication.
    #[tokio::test]
    async fn a_link_batch_resolves_dependencies_supplied_by_the_same_batch() {
        use lash_core_store::artifact_referrer::{ArtifactReferrer, HostArtifactPin};
        let store = CountedStorage::default();
        let document = lash_kernel_value::names::DocumentId::from_bytes([0x47; 32]);
        store
            .records
            .lock()
            .expect("fixture")
            .insert(document.to_string(), vec![7]);
        let child = lash_kernel_value::package::Link {
            helper_fills: Default::default(),
            document,
            fills: BTreeMap::new(),
        };
        let parent = lash_kernel_value::package::Link {
            helper_fills: Default::default(),
            document,
            fills: [(
                lash_kernel_value::names::ImportSlot(0),
                lash_kernel_value::package::Fill::Export {
                    link: child.identity(),
                    export: lash_kernel_value::names::ExportName {
                        name: "value".into(),
                        member: None,
                    },
                },
            )]
            .into(),
        };
        let child = LinkRecord {
            id: child.identity(),
            link: child,
        };
        let parent = LinkRecord {
            id: parent.identity(),
            link: parent,
        };
        let claim = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
            .expect("fixture");
        store
            .acquire_batch(
                &claim,
                &[
                    PackageHold::Link {
                        record: &parent,
                        artifacts: &[],
                    },
                    PackageHold::Link {
                        record: &child,
                        artifacts: &[],
                    },
                ],
            )
            .await
            .expect("new dependency and parent are acquired together");
        let records = store.records.lock().expect("fixture");
        assert!(records.contains_key(&link_reference(&parent.id)));
        assert!(records.contains_key(&link_reference(&child.id)));
        assert!(
            store.reads.lock().expect("fixture").is_empty(),
            "new records cannot be read before the batch commits"
        );
    }

    /// FIG-7135: retaining code discovers dependencies from package records,
    /// without hydrating terminal code bodies. Their presence is checked by
    /// the same atomic hold that acquires their identities.
    #[tokio::test]
    async fn retaining_a_link_never_materializes_terminal_artifact_bodies() {
        use lash_core_store::artifact_referrer::{ArtifactReferrer, HostArtifactPin};
        let store = CountedStorage::default();
        let document = lash_kernel_value::names::DocumentId::from_bytes([0x71; 32]);
        let link = lash_kernel_value::package::Link {
            helper_fills: Default::default(),
            document,
            fills: BTreeMap::new(),
        };
        let reference = link_reference(&link.identity());
        let stored = StoredLink {
            record: LinkRecord {
                id: link.identity(),
                link,
            },
            artifacts: Default::default(),
        };
        store.records.lock().expect("work-law fixture").extend([
            (
                reference.clone(),
                encode(&stored).expect("work-law fixture"),
            ),
            (document.to_string(), vec![0x71; 1 << 20]),
        ]);
        let claim = ReferrerClaim::unguarded(ArtifactReferrer::HostPin(HostArtifactPin::mint()))
            .expect("work-law fixture");
        for _ in 0..3 {
            acquire_content(&store, &claim, &reference)
                .await
                .expect("work-law fixture");
        }
        assert_eq!(
            *store.reads.lock().expect("work-law fixture"),
            vec![reference.clone(); 3],
            "retention must not read leaf payloads to discover that they have no dependencies"
        );
        assert!(
            store
                .holds
                .lock()
                .expect("work-law fixture")
                .contains(&document.to_string())
        );
        // Cleanup between discovery and acquisition cannot turn an absent
        // leaf into a successful hold: the transaction remains the oracle.
        store
            .records
            .lock()
            .expect("work-law fixture")
            .remove(&document.to_string());
        assert!(matches!(
            acquire_content(&store, &claim, &reference).await,
            Err(DurableError::Package(PackageRefusal::Missing(missing)))
                if missing == document.to_string()
        ));
    }
}
