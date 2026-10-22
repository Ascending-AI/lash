//! Retain immutable Documents/Links before publishing references. Cold readers validate stored bytes afresh.
use lash_core::{ArtifactName, ArtifactStoreId, ReferrerClaim};
use lash_core_store::packages::{LinkRecord, PackageRecord};
use lash_durable::packages::PackageStore;
use lash_execution_contracts::module_artifacts::{
    ArtifactStoreError, ModuleArtifactBody, ModuleArtifactHold, ModuleArtifactStore,
};
use lash_execution_contracts::process::linking::*;
use lash_kernel_doc::{Document, DocumentBytes, IdentifiedDocument, SourceMap};

use lash_kernel_package::{LinkRefusal, LinkedContent, Namespace, ReachSelection, RetainedContent};
use lash_vocabulary::data_limits::KernelDataLimits;
use lash_vocabulary::names::DocumentId;
use lash_vocabulary::names::SourceMapId;
use lash_vocabulary::tokens::{TokenCodec as _, TokenWriter, logical_digest};
use lash_vocabulary::{names::*, package::*};
use std::collections::{BTreeMap, BTreeSet};
use std::sync::{Arc, Mutex, OnceLock};

pub(crate) fn document_artifact(document: &DocumentId) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::KernelDocument,
        artifact_ref: document.to_string(),
    }
}
pub(crate) fn link_artifact(link: LinkId) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::KernelDocument,
        artifact_ref: format!("link:v1:{link}"),
    }
}
pub(crate) fn package_artifact(package: PackageId) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::KernelDocument,
        artifact_ref: format!("package:v1:{package}"),
    }
}
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unretained {
    Missing {
        artifact: ArtifactName,
    },
    Corrupt {
        artifact: ArtifactName,
        message: String,
    },
}
#[derive(Clone, Debug)]
pub enum StoredSourceMap {
    Read { map: SourceMap, source: String },
    Unretained(Unretained),
}
#[derive(Clone)]
pub struct KernelDocuments {
    store: Arc<dyn ModuleArtifactStore>,
    packages: Arc<dyn PackageStore>,
    /// Pure deterministic preparations only; never a replacement for an existing stored read.
    prepared: Arc<Mutex<DocumentPreparations>>,
}
// Pure preparations belong to the same document owner as unpublished Links.
// A per-content slot serializes first preparation without holding the inventory
// mutex during decoding or checking. It contains no store or referrer authority.
type HelperPreparation = OnceLock<Result<Arc<PreparedHelperRelease>, EnginePortError>>;
#[derive(Default)]
struct DocumentPreparations {
    linked: LinkedContent,
    helpers: BTreeMap<[u8; 32], Arc<HelperPreparation>>,
}
struct PreparedHelperRelease {
    documents: Vec<IdentifiedDocument>,
    links: Vec<LinkRecord>,
    packages: Vec<PackageRecord>,
    labels: BTreeMap<PackageId, String>,
}

impl PreparedHelperRelease {
    fn new(
        content: &LinkedContent,
        labels: &BTreeMap<PackageId, String>,
    ) -> Result<Self, EnginePortError> {
        for (id, document) in &content.documents {
            if document.identity() != *id {
                return Err(map_link_refusal(LinkRefusal::InvalidDocument {
                    message: "helper Document hash differs".into(),
                }));
            }
            lash_kernel_doc::validate_document(document, &document.functions).map_err(|e| {
                map_link_refusal(LinkRefusal::InvalidDocument {
                    message: e.to_string(),
                })
            })?;
        }
        for id in content.links.keys() {
            content.retain_reach(*id).map_err(map_link_refusal)?;
        }
        for (id, package) in &content.packages {
            if package.identity() != *id {
                return Err(map_link_refusal(LinkRefusal::InvalidDocument {
                    message: "helper Package hash differs".into(),
                }));
            }
            for member in package.members.values() {
                if let MemberKind::Export { link, export } = &member.kind {
                    content.export(*link, export).map_err(map_link_refusal)?;
                }
            }
        }
        let documents = content
            .documents
            .values()
            .map(|document| {
                let document = DocumentBytes::from(document.clone())
                    .decode(KernelDataLimits::standard())
                    .map_err(|error| {
                        map_link_refusal(LinkRefusal::InvalidDocument {
                            message: error.to_string(),
                        })
                    })?;
                lash_kernel_check::derive(&document, &document.functions).map_err(|error| {
                    map_link_refusal(LinkRefusal::InvalidDocument {
                        message: error.to_string(),
                    })
                })?;
                Ok(document)
            })
            .collect::<Result<Vec<_>, EnginePortError>>()?;
        let mut links = Vec::with_capacity(content.links.len());
        let mut pending: BTreeSet<_> = content.links.keys().copied().collect();
        let mut written = BTreeSet::new();
        while !pending.is_empty() {
            let ready = pending
                .iter()
                .copied()
                .find(|id| {
                    content.links[id].fills.values().all(|fill| match fill {
                        Fill::Export { link, .. } => written.contains(link),
                        _ => true,
                    })
                })
                .ok_or_else(|| {
                    map_link_refusal(LinkRefusal::InvalidDocument {
                        message: "cyclic helper Link graph".into(),
                    })
                })?;
            links.push(LinkRecord {
                id: ready,
                link: content.links[&ready].clone(),
            });
            pending.remove(&ready);
            written.insert(ready);
        }
        for (id, json) in labels {
            let package = content.packages.get(id).ok_or_else(|| {
                map_link_refusal(LinkRefusal::InvalidDocument {
                    message: "labels name an absent Package".into(),
                })
            })?;
            lash_kernel_package::read_labels(content, package, json).map_err(|error| {
                map_link_refusal(LinkRefusal::InvalidDocument {
                    message: error.to_string(),
                })
            })?;
        }
        Ok(Self {
            documents,
            links,
            packages: content
                .packages
                .iter()
                .map(|(id, package)| PackageRecord {
                    id: *id,
                    package: package.clone(),
                })
                .collect(),
            labels: labels.clone(),
        })
    }
}

#[derive(Default)]
pub(crate) struct ProcessPreparations;
impl ProcessPreparations {
    pub(crate) fn bind(&self, engine: super::KernelProcessEngine) -> super::KernelProcessEngine {
        engine
    }
}
fn artifact_error(error: ArtifactStoreError) -> EnginePortError {
    match error {
        ArtifactStoreError::ArtifactMissing { artifact_ref } => EnginePortError::Missing {
            artifact: ArtifactName {
                store: ArtifactStoreId::KernelDocument,
                artifact_ref,
            },
        },
        ArtifactStoreError::ReferrerEnded { .. } => EnginePortError::Fenced,
        error => EnginePortError::Unavailable {
            message: error.to_string(),
        },
    }
}
fn store_error(error: lash_durable::DurableError) -> EnginePortError {
    use lash_core_store::packages::PackageRefusal;
    match error {
        lash_durable::DurableError::Package(
            PackageRefusal::Missing(artifact_ref)
            | PackageRefusal::CarryMissing { artifact_ref, .. },
        ) => EnginePortError::Missing {
            artifact: ArtifactName {
                store: ArtifactStoreId::KernelDocument,
                artifact_ref,
            },
        },
        lash_durable::DurableError::Package(
            PackageRefusal::Identity(artifact_ref) | PackageRefusal::Immutable(artifact_ref),
        ) => EnginePortError::Corrupt {
            artifact: ArtifactName {
                store: ArtifactStoreId::KernelDocument,
                artifact_ref,
            },
            message: "immutable package content mismatch".into(),
        },
        lash_durable::DurableError::Package(PackageRefusal::ReferrerEnded(_)) => {
            EnginePortError::Fenced
        }
        lash_durable::DurableError::Package(error) => {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                message: error.to_string(),
            })
        }
        error => EnginePortError::Unavailable {
            message: error.to_string(),
        },
    }
}
pub fn map_link_refusal(error: LinkRefusal) -> EnginePortError {
    let cause = match error {
        LinkRefusal::MissingImport { slot, name } => AdmissionRefusal::MissingImport { slot, name },
        LinkRefusal::KindMismatch { slot, name } => AdmissionRefusal::KindMismatch { slot, name },
        LinkRefusal::SignatureMismatch { slot, name } => {
            AdmissionRefusal::SignatureMismatch { slot, name }
        }
        LinkRefusal::ConventionMismatch { slot, name } => {
            AdmissionRefusal::ConventionMismatch { slot, name }
        }
        LinkRefusal::NotVisible { name } => AdmissionRefusal::NotVisible { name },
        LinkRefusal::PathConflict { path } => AdmissionRefusal::PathConflict { path },
        LinkRefusal::NotOwner { path } => AdmissionRefusal::NotOwner { path },
        LinkRefusal::UndeclaredDeferred { path } => AdmissionRefusal::UndeclaredDeferred { path },
        LinkRefusal::ContractMismatch {
            use_,
            pinned,
            offered,
        } => AdmissionRefusal::ContractMismatch {
            use_,
            pinned,
            offered,
        },
        LinkRefusal::MissingMain => AdmissionRefusal::MissingMain,
        LinkRefusal::NotFunction { export } => AdmissionRefusal::NotFunction { export },
        LinkRefusal::NotDataOnly { export } => AdmissionRefusal::NotDataOnly { export },
        LinkRefusal::MissingDocument { document } => {
            return EnginePortError::Missing {
                artifact: document_artifact(&document),
            };
        }
        LinkRefusal::MissingLink { link } => {
            return EnginePortError::Missing {
                artifact: link_artifact(link),
            };
        }
        LinkRefusal::InvalidDocument { message } => AdmissionRefusal::InvalidDocument { message },
    };
    EnginePortError::Refused(cause)
}
pub(super) fn selection(selector: &ReachSelector) -> ReachSelection {
    match selector {
        ReachSelector::AllExports => ReachSelection::AllExports,
        ReachSelector::Callable { function } => ReachSelection::Callable(*function),
        ReachSelector::Start(start) => ReachSelection::Start(start.clone()),
    }
}
fn namespace(snapshot: &NamespaceSnapshot) -> Namespace<'_> {
    Namespace {
        advertisements: snapshot
            .advertisements
            .iter()
            .map(|ad| (&ad.publication.context, &ad.advertisement))
            .collect(),
    }
}
impl KernelDocuments {
    pub fn new(store: Arc<dyn ModuleArtifactStore>, packages: Arc<dyn PackageStore>) -> Self {
        Self {
            store,
            packages,
            prepared: Arc::new(Mutex::new(DocumentPreparations::default())),
        }
    }
    /// Bootstrap build-owned helper content under the receiving owner's existing claim.
    /// Advertisement acceptance follows this operation; cancellation leaves only that claim's debt.
    pub async fn hold_helper_release(
        &self,
        claim: &ReferrerClaim,
        content: &LinkedContent,
        labels: &BTreeMap<PackageId, String>,
    ) -> Result<(), EnginePortError> {
        // Hash actual canonical content, including supplied ids and sidecars:
        // a mismatched body or label cannot hit a previously validated release.
        let mut out = TokenWriter::new();
        out.seq(4);
        content.documents.encode(&mut out);
        content.links.encode(&mut out);
        content.packages.encode(&mut out);
        labels.encode(&mut out);
        let identity = logical_digest(&out.into_bytes());
        let slot = self
            .prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .helpers
            .entry(identity)
            .or_default()
            .clone();
        let release = slot
            .get_or_init(|| PreparedHelperRelease::new(content, labels).map(Arc::new))
            .clone()?;
        for document in &release.documents {
            let artifact = document_artifact(&document.id());
            self.store
                .hold_module_artifacts(
                    claim,
                    &[ModuleArtifactHold::Publish {
                        module_ref: &artifact.artifact_ref,
                        body: ModuleArtifactBody::Document(document.body()),
                    }],
                )
                .await
                .map_err(artifact_error)?;
        }
        for record in &release.links {
            self.packages
                .put_link(claim, record, &[document_artifact(&record.link.document)])
                .await
                .map_err(store_error)?;
        }
        for record in &release.packages {
            self.packages
                .put_package(claim, record)
                .await
                .map_err(store_error)?;
        }
        for (id, json) in &release.labels {
            self.packages
                .put_labels(claim, id, json)
                .await
                .map_err(store_error)?;
        }
        Ok(())
    }
    pub async fn admit_document(
        &self,
        claim: &ReferrerClaim,
        bytes: &DocumentBytes,
    ) -> Result<DocumentId, EnginePortError> {
        let document = bytes.decode(KernelDataLimits::standard()).map_err(|e| {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                message: e.to_string(),
            })
        })?;
        lash_kernel_check::derive(&document, &document.functions).map_err(|e| {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                message: e.to_string(),
            })
        })?;
        let artifact = document_artifact(&document.id());
        self.store
            .hold_module_artifacts(
                claim,
                &[ModuleArtifactHold::Publish {
                    module_ref: &artifact.artifact_ref,
                    body: ModuleArtifactBody::Document(document.body()),
                }],
            )
            .await
            .map_err(artifact_error)?;
        Ok(document.id())
    }
    pub async fn get_shared(
        &self,
        id: &DocumentId,
    ) -> Result<Option<IdentifiedDocument>, EnginePortError> {
        let artifact = document_artifact(id);
        let Some(bytes) = self
            .store
            .get_module_artifact(&artifact.artifact_ref)
            .await
            .map_err(artifact_error)?
        else {
            return Ok(None);
        };
        let document = IdentifiedDocument::decode_standard(&bytes).map_err(|error| {
            EnginePortError::Corrupt {
                artifact: artifact.clone(),
                message: error.to_string(),
            }
        })?;
        if document.id() != *id {
            return Err(EnginePortError::Corrupt {
                artifact,
                message: "Document content hash differs".into(),
            });
        }
        lash_kernel_doc::validate_document(&document, &document.functions).map_err(|e| {
            EnginePortError::Corrupt {
                artifact: document_artifact(id),
                message: e.to_string(),
            }
        })?;
        Ok(Some(document))
    }
    pub async fn get(&self, id: &DocumentId) -> Result<Option<Document>, EnginePortError> {
        Ok(self
            .get_shared(id)
            .await?
            .map(IdentifiedDocument::into_document))
    }
    /// Full cold-load from immutable store records, including currently unused fills.
    pub async fn load_linked(&self, root: LinkId) -> Result<LinkedContent, EnginePortError> {
        let mut content = LinkedContent::default();
        let mut pending = vec![root];
        while let Some(id) = pending.pop() {
            if content.links.contains_key(&id) {
                continue;
            }
            let record = self
                .packages
                .link(&id)
                .await
                .map_err(store_error)?
                .ok_or_else(|| EnginePortError::Missing {
                    artifact: link_artifact(id),
                })?;
            if record.id != id || record.link.identity() != id {
                return Err(EnginePortError::Corrupt {
                    artifact: link_artifact(id),
                    message: "Link hash differs".into(),
                });
            }
            let document =
                self.get(&record.link.document)
                    .await?
                    .ok_or_else(|| EnginePortError::Missing {
                        artifact: document_artifact(&record.link.document),
                    })?;
            content.documents.insert(record.link.document, document);
            pending.extend(record.link.fills.values().filter_map(|f| match f {
                Fill::Export { link, .. } => Some(*link),
                _ => None,
            }));
            content.links.insert(id, record.link);
        }
        for link in content.links.values() {
            content.check_link(link).map_err(map_link_refusal)?;
        }
        content.retain_reach(root).map_err(map_link_refusal)?;
        Ok(content)
    }
    async fn content_for(
        &self,
        document: DocumentId,
        snapshot: &NamespaceSnapshot,
    ) -> Result<LinkedContent, EnginePortError> {
        let mut content = LinkedContent::default();
        let doc = self
            .get(&document)
            .await?
            .ok_or_else(|| EnginePortError::Missing {
                artifact: document_artifact(&document),
            })?;
        content.documents.insert(document, doc);
        for ad in &snapshot.advertisements {
            if let PackageAdvertisement::Enumerated(package) = &ad.advertisement {
                for member in package.members.values() {
                    if let MemberKind::Export { link, .. } = member.kind
                        && !content.links.contains_key(&link)
                    {
                        let loaded = self.load_linked(link).await?;
                        content.documents.extend(loaded.documents);
                        content.links.extend(loaded.links);
                    }
                }
            }
        }
        Ok(content)
    }
    /// A runtime-authored successor Link is structurally checked against cold immutable dependencies.
    pub(crate) async fn prepare_link(&self, link: Link) -> Result<(), EnginePortError> {
        let mut content = LinkedContent::default();
        let document = self
            .get(&link.document)
            .await?
            .ok_or_else(|| EnginePortError::Missing {
                artifact: document_artifact(&link.document),
            })?;
        content
            .insert_document(document)
            .map_err(map_link_refusal)?;
        for fill in link.fills.values() {
            if let Fill::Export { link, .. } = fill {
                let child = self.load_linked(*link).await?;
                content.documents.extend(child.documents);
                content.links.extend(child.links);
            }
        }
        content.check_link(&link).map_err(map_link_refusal)?;
        self.prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .linked
            .links
            .insert(link.identity(), link);
        Ok(())
    }
    pub async fn fulfil(&self, request: &FulfilRequest) -> Result<Fulfilled, EnginePortError> {
        let mut content = self
            .content_for(request.document, &request.snapshot)
            .await?;
        let (own, attachment) = match &request.purpose {
            LinkPurpose::Cell => (None, None),
            LinkPurpose::Workflow { attachment } => (None, Some(attachment)),
            LinkPurpose::PackageCode { path } => (Some(path), None),
        };
        let link = namespace(&request.snapshot)
            .fulfil(
                &content,
                request.document,
                &request.context,
                own,
                attachment,
            )
            .map_err(map_link_refusal)?;
        let id = link.identity();
        content.links.insert(id, link.clone());
        let inspection = inspection(&content, id, &request.selector)?;
        {
            let mut prepared = self
                .prepared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            prepared.linked.links.insert(id, link.clone());
        }
        Ok(Fulfilled {
            content: link,
            inspection,
        })
    }
    /// Retain accepted publication records intact. The store atomically retains
    /// every imported publication and code dependency under this same claim.
    /// Prove a callable origin from trusted accepted publication keys. Stored
    /// hashes and a PublicationRecord's diagnostic code_links are never roots.
    pub async fn validate_published_origin(
        &self,
        link: LinkId,
        origin: &CallableOrigin,
    ) -> Result<(), EnginePortError> {
        let mut pending: Vec<_> = origin.publications.iter().cloned().collect();
        let mut seen = BTreeSet::new();
        let mut roots = BTreeSet::new();
        let mut advertisements = Vec::new();
        while let Some(key) = pending.pop() {
            if !seen.insert(key.clone()) {
                continue;
            }
            let artifact = ArtifactName {
                store: ArtifactStoreId::KernelDocument,
                artifact_ref: lash_durable::packages::publication_reference(&key),
            };
            let record = self
                .packages
                .publication(&key)
                .await
                .map_err(store_error)?
                .ok_or_else(|| EnginePortError::Missing {
                    artifact: artifact.clone(),
                })?;
            if record.key() != key {
                return Err(EnginePortError::Corrupt {
                    artifact,
                    message: "publication key mismatch".into(),
                });
            }
            pending.extend(record.imported_publications.iter().cloned());
            if let PublishedContent::Package(id) = record.content {
                let package = self
                    .packages
                    .package(&id)
                    .await
                    .map_err(store_error)?
                    .ok_or_else(|| EnginePortError::Missing {
                        artifact: package_artifact(id),
                    })?;
                if package.id != id
                    || package.package.identity() != id
                    || package.package.path != record.path
                {
                    return Err(EnginePortError::Corrupt {
                        artifact: package_artifact(id),
                        message: "published Package differs from its key or path".into(),
                    });
                }
                roots.extend(package.package.members.values().filter_map(|member| {
                    match (&member.visibility, &member.kind) {
                        (Visibility::Public, MemberKind::Export { link, .. }) => Some(*link),
                        _ => None,
                    }
                }));
                if origin.publications.contains(&key) {
                    advertisements.push((
                        record.context,
                        PackageAdvertisement::Enumerated(package.package),
                    ));
                }
            } else if let PublishedContent::Deferred(package) = record.content
                && origin.publications.contains(&key)
            {
                advertisements.push((record.context, PackageAdvertisement::Deferred(package)));
            }
        }
        let namespace = Namespace {
            advertisements: advertisements
                .iter()
                .map(|(context, ad)| (context, ad))
                .collect(),
        };
        let mut authored_reaches = Vec::new();
        for root in &origin.authored {
            let content = self.load_linked(*root).await?;
            namespace
                .validate_authored_origin(&content, content.link(*root).map_err(map_link_refusal)?)
                .map_err(map_link_refusal)?;
            authored_reaches.push(content);
        }
        if authored_reaches
            .iter()
            .any(|content| content.links.contains_key(&link))
        {
            return Ok(());
        }
        for root in roots {
            let content = self.load_linked(root).await?;
            if content.links.contains_key(&link) {
                return Ok(());
            }
        }
        Err(EnginePortError::Refused(
            AdmissionRefusal::UnpublishedOrigin { link },
        ))
    }
    /// Keep authenticated callable provenance under the same start/host claim.
    pub async fn hold_callable_origin(
        &self,
        claim: &ReferrerClaim,
        origin: &CallableOrigin,
    ) -> Result<(), EnginePortError> {
        for key in &origin.publications {
            self.packages
                .acquire_publication(claim, key)
                .await
                .map_err(store_error)?;
        }
        for link in &origin.authored {
            self.hold(claim, &ContentRef::Link(*link)).await?;
        }
        Ok(())
    }
    pub async fn hold_publications(
        &self,
        claim: &ReferrerClaim,
        snapshot: &NamespaceSnapshot,
    ) -> Result<(), EnginePortError> {
        for accepted in &snapshot.advertisements {
            let record = &accepted.publication;
            let key = record.key();
            let stored = self.packages.publication(&key).await.map_err(store_error)?;
            if stored.as_ref() != Some(record) {
                return Err(EnginePortError::Corrupt {
                    artifact: ArtifactName {
                        store: ArtifactStoreId::KernelDocument,
                        artifact_ref: lash_durable::packages::publication_reference(&key),
                    },
                    message: "accepted publication is absent or differs from the retained record"
                        .into(),
                });
            }
            self.packages
                .acquire_publication(claim, &key)
                .await
                .map_err(store_error)?;
        }
        Ok(())
    }
    pub async fn inspect_link(
        &self,
        link: LinkId,
        selector: &ReachSelector,
    ) -> Result<LinkInspection, EnginePortError> {
        inspection(&self.load_linked(link).await?, link, selector)
    }
    /// Retain existing content by its stored pointer. Only a fresh, unpublished preparation may write a Link.
    pub async fn hold(
        &self,
        claim: &ReferrerClaim,
        reference: &ContentRef,
    ) -> Result<(), EnginePortError> {
        let artifact = match reference {
            ContentRef::Document(id) => {
                self.get_shared(id)
                    .await?
                    .ok_or_else(|| EnginePortError::Missing {
                        artifact: document_artifact(id),
                    })?;
                document_artifact(id)
            }
            ContentRef::Package(id) => {
                let record = self
                    .packages
                    .package(id)
                    .await
                    .map_err(store_error)?
                    .ok_or_else(|| EnginePortError::Missing {
                        artifact: package_artifact(*id),
                    })?;
                if record.id != *id || record.package.identity() != *id {
                    return Err(EnginePortError::Corrupt {
                        artifact: package_artifact(*id),
                        message: "Package hash differs".into(),
                    });
                }
                for member in record.package.members.values() {
                    if let MemberKind::Export { link, .. } = member.kind {
                        self.load_linked(link).await?;
                    }
                }
                package_artifact(*id)
            }
            ContentRef::Link(id) => {
                if self.packages.link(id).await.map_err(store_error)?.is_some() {
                    self.load_linked(*id).await?;
                } else {
                    let link = self
                        .prepared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .linked
                        .links
                        .get(id)
                        .cloned()
                        .ok_or_else(|| EnginePortError::Missing {
                            artifact: link_artifact(*id),
                        })?;
                    let document = self.get(&link.document).await?.ok_or_else(|| {
                        EnginePortError::Missing {
                            artifact: document_artifact(&link.document),
                        }
                    })?;
                    let mut content = LinkedContent::default();
                    content
                        .insert_document(document)
                        .map_err(map_link_refusal)?;
                    for fill in link.fills.values() {
                        if let Fill::Export { link, .. } = fill {
                            let descendant = self.load_linked(*link).await?;
                            content.documents.extend(descendant.documents);
                            content.links.extend(descendant.links);
                        }
                    }
                    content.check_link(&link).map_err(map_link_refusal)?;
                    self.packages
                        .put_link(
                            claim,
                            &LinkRecord {
                                id: *id,
                                link: link.clone(),
                            },
                            &[document_artifact(&link.document)],
                        )
                        .await
                        .map_err(store_error)?;
                    self.prepared
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner)
                        .linked
                        .links
                        .remove(id);
                    return Ok(());
                }
                link_artifact(*id)
            }
        };
        self.store
            .acquire_module_artifact(claim, &artifact.artifact_ref)
            .await
            .map_err(artifact_error)
    }
    pub async fn execution_document(
        &self,
        start: &LinkedStart,
    ) -> Result<lash_kernel_check::WorkflowDocument, EnginePortError> {
        let content = self.load_linked(start.link).await?;
        content
            .inspect(start.link, &ReachSelection::Start(start.selector.clone()))
            .map_err(map_link_refusal)?;
        let document = content
            .document(content.link(start.link).map_err(map_link_refusal)?.document)
            .map_err(map_link_refusal)?
            .clone();
        lash_kernel_check::WorkflowDocument::entered(document, start.clone()).map_err(|e| {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                message: e.to_string(),
            })
        })
    }
    pub async fn load(&self, reference: &ContentRef) -> Result<LoadedContent, EnginePortError> {
        let material = match reference {
            ContentRef::Document(id) => lash_execution_contracts::process::ProcessDocument::new(
                self.get(id)
                    .await?
                    .ok_or_else(|| EnginePortError::Missing {
                        artifact: document_artifact(id),
                    })?,
            ),
            ContentRef::Link(id) => {
                let content = self.load_linked(*id).await?;
                lash_execution_contracts::process::ProcessDocument::new(
                    content.link(*id).map_err(map_link_refusal)?.clone(),
                )
            }
            ContentRef::Package(id) => {
                let record = self
                    .packages
                    .package(id)
                    .await
                    .map_err(store_error)?
                    .ok_or_else(|| EnginePortError::Missing {
                        artifact: package_artifact(*id),
                    })?;
                if record.id != *id || record.package.identity() != *id {
                    return Err(EnginePortError::Corrupt {
                        artifact: package_artifact(*id),
                        message: "Package hash differs".into(),
                    });
                }
                lash_execution_contracts::process::ProcessDocument::new(record.package)
            }
        };
        Ok(LoadedContent {
            reference: reference.clone(),
            material,
        })
    }
}
pub(super) fn inspection(
    content: &LinkedContent,
    root: LinkId,
    selector: &ReachSelector,
) -> Result<LinkInspection, EnginePortError> {
    let reach = content
        .inspect(root, &selection(selector))
        .map_err(map_link_refusal)?;
    Ok(LinkInspection {
        link: root,
        document: content.link(root).map_err(map_link_refusal)?.document,
        selector: selector.clone(),
        operations: reach
            .operations
            .into_iter()
            .map(|(use_, need)| {
                (
                    use_,
                    match need {
                        lash_kernel_package::OperationNeed::Fixed { name, requirement } => {
                            OperationNeed::Fixed { name, requirement }
                        }
                        lash_kernel_package::OperationNeed::Deferred { name, grant } => {
                            OperationNeed::Deferred { name, grant }
                        }
                    },
                )
            })
            .collect(),
        machine: reach.machine,
        signature: reach.signature,
        artifacts: reach
            .artifacts
            .into_iter()
            .map(|a| match a {
                RetainedContent::Document(id) => document_artifact(&id),
                RetainedContent::Link(id) => link_artifact(id),
                RetainedContent::Package(id) => package_artifact(id),
            })
            .collect(),
    })
}

fn source_map_artifact(map: &SourceMapId) -> ArtifactName {
    ArtifactName {
        store: lash_core::ArtifactStoreId::KernelDocument,
        artifact_ref: format!("source-map:{map}"),
    }
}

fn source_artifact(digest: &[u8; 32]) -> ArtifactName {
    use std::fmt::Write as _;
    let mut hex = String::with_capacity(64);
    for byte in digest {
        let _ = write!(hex, "{byte:02x}");
    }
    ArtifactName {
        store: lash_core::ArtifactStoreId::KernelDocument,
        artifact_ref: format!("source:{hex}"),
    }
}

impl KernelDocuments {
    pub async fn publish_source_map(
        &self,
        claim: &ReferrerClaim,
        document: &Document,
        map: &SourceMap,
        source: &str,
    ) -> Result<SourceMapId, EnginePortError> {
        map.validate(document, source).map_err(|e| {
            EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                message: e.to_string(),
            })
        })?;
        let identity = map.identity();
        let map_bytes = map.canonical_bytes();
        self.store
            .hold_module_artifacts(
                claim,
                &[
                    ModuleArtifactHold::Publish {
                        module_ref: &source_artifact(&map.source).artifact_ref,
                        body: ModuleArtifactBody::Bytes(source.as_bytes()),
                    },
                    ModuleArtifactHold::Publish {
                        module_ref: &source_map_artifact(&identity).artifact_ref,
                        body: ModuleArtifactBody::Bytes(&map_bytes),
                    },
                ],
            )
            .await
            .map_err(artifact_error)?;
        Ok(identity)
    }

    /// The map stored under `identity` and the source it maps.
    ///
    /// # Errors
    ///
    /// The store's failure to answer.
    pub async fn source_map(
        &self,
        identity: &SourceMapId,
    ) -> Result<StoredSourceMap, EnginePortError> {
        let artifact = source_map_artifact(identity);
        let Some(bytes) = self
            .store
            .get_module_artifact(&artifact.artifact_ref)
            .await
            .map_err(artifact_error)?
        else {
            return Ok(StoredSourceMap::Unretained(Unretained::Missing {
                artifact,
            }));
        };
        let corrupt = |artifact: ArtifactName, message: String| {
            Ok(StoredSourceMap::Unretained(Unretained::Corrupt {
                artifact,
                message,
            }))
        };
        let map = match SourceMap::from_canonical_bytes(&bytes) {
            Ok(map) => map,
            Err(message) => return corrupt(artifact, message),
        };
        if map.identity() != *identity {
            return corrupt(artifact, "the source map is another map".to_owned());
        }
        let artifact = source_artifact(&map.source);
        let Some(bytes) = self
            .store
            .get_module_artifact(&artifact.artifact_ref)
            .await
            .map_err(artifact_error)?
        else {
            return Ok(StoredSourceMap::Unretained(Unretained::Missing {
                artifact,
            }));
        };
        let Ok(source) = String::from_utf8(bytes) else {
            return corrupt(artifact, "the source is not UTF-8".to_owned());
        };
        let checked = match self.get_shared(&map.document).await? {
            Some(document) => map.validate_identified(&document, &source),
            None => map.validate_source(&source),
        };
        match checked {
            Ok(()) => Ok(StoredSourceMap::Read { map, source }),
            Err(refusal) => corrupt(artifact, refusal.to_string()),
        }
    }
}

pub(super) use map_link_refusal as refused;
