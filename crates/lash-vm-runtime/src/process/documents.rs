//! Retain immutable Documents/Links before publishing references. Cold readers validate stored bytes afresh.
use lash_core::{ArtifactName, ArtifactStoreId, ReferrerClaim};
use lash_core_store::packages::LinkRecord;
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
use lash_vocabulary::{names::*, package::*};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::sync::{Arc, Mutex, OnceLock};
use tracing::Instrument as _;

pub(super) async fn artifact_phase<T>(
    phase: &'static str,
    span: tracing::Span,
    operation: impl std::future::Future<Output = Result<T, EnginePortError>>,
) -> Result<T, EnginePortError> {
    async {
        operation.await.inspect_err(|error| {
            let kind = match error {
                EnginePortError::Refused(_) => "admission_refused",
                EnginePortError::PackageRefused(_) => "package_refused",
                EnginePortError::Missing { .. } => "artifact_missing",
                EnginePortError::Corrupt { .. } => "artifact_corrupt",
                EnginePortError::Fenced => "pin_released",
                EnginePortError::Unsupported { .. } => "unsupported",
                EnginePortError::StoreRefused(_) => "store_refused",
                EnginePortError::StoreUnavailable { .. } | EnginePortError::Unavailable { .. } => {
                    return;
                }
            };
            tracing::debug!(event = "artifact_refused", phase, refusal_kind = kind);
        })
    }
    .instrument(span)
    .await
}

fn store_write_refused(error: &ArtifactStoreError) {
    if let Some(kind) = error.refusal_code() {
        tracing::debug!(
            event = "artifact_refused",
            phase = "store_write",
            refusal_kind = kind
        );
    }
}

pub(crate) fn document_artifact(document: &DocumentId) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::KernelDocument,
        artifact_ref: document.to_string(),
    }
}
pub(crate) fn link_artifact(link: LinkId) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::KernelDocument,
        artifact_ref: lash_durable::packages::link_reference(&link),
    }
}
pub(crate) fn package_artifact(package: PackageId) -> ArtifactName {
    ArtifactName {
        store: ArtifactStoreId::KernelDocument,
        artifact_ref: lash_durable::packages::package_reference(&package),
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
    helpers: Arc<lash_kernel_doc::BuildHelpers>,
    /// Pure deterministic preparations only; never a replacement for an existing stored read.
    prepared: Arc<Mutex<DocumentPreparations>>,
}
// Pure preparations belong to the same document owner as unpublished Links.
#[derive(Default)]
struct DocumentPreparations {
    linked: LinkedContent,
    documents: VecDeque<Arc<PreparedDocument>>,
    document_bytes: usize,
    #[cfg(test)]
    validations: usize,
}
// The existing document owner retains at most sixteen preparations and
// eight MiB of encoded content, including the input and decoded body's copy.
const DOCUMENTS: usize = 16;
const DOCUMENT_BYTES: usize = 8 * 1024 * 1024;

struct PreparedDocument {
    id: DocumentId,
    bytes: Arc<[u8]>,
    checked: OnceLock<Result<IdentifiedDocument, EnginePortError>>,
    decoded: OnceLock<Result<IdentifiedDocument, lash_vocabulary::tokens::TokenError>>,
    derived: OnceLock<Result<(), EnginePortError>>,
}

impl DocumentPreparations {
    fn document(&mut self, id: &DocumentId, bytes: &[u8]) -> Option<Arc<PreparedDocument>> {
        let at = self
            .documents
            .iter()
            .position(|prepared| prepared.id == *id && prepared.bytes.as_ref() == bytes)?;
        let prepared = self.documents.remove(at)?;
        self.documents.push_back(Arc::clone(&prepared));
        Some(prepared)
    }
}

#[derive(Default)]
pub(crate) struct ProcessPreparations {
    prepared: Arc<Mutex<DocumentPreparations>>,
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
        ArtifactStoreError::StoreFault(fault) => EnginePortError::StoreUnavailable { fault },
        ArtifactStoreError::Backend(message) => EnginePortError::StoreUnavailable {
            fault: lash_core::store::StoreFault::Backend { message },
        },
        ArtifactStoreError::StoreRefusal(refusal) => EnginePortError::StoreRefused(refusal),
        ArtifactStoreError::Incompatible { refusal } => {
            EnginePortError::StoreRefused(lash_core::store::StoreRefusal::Incompatible { refusal })
        }
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
        error => error.into(),
    }
}
pub fn map_link_refusal(error: LinkRefusal) -> EnginePortError {
    let cause = match error {
        LinkRefusal::SignatureRequired(source) => AdmissionRefusal::SignatureRequired { source },
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
    pub fn new(
        store: Arc<dyn ModuleArtifactStore>,
        packages: Arc<dyn PackageStore>,
        helpers: Arc<lash_kernel_doc::BuildHelpers>,
    ) -> Self {
        Self {
            store,
            packages,
            helpers,
            prepared: Arc::new(Mutex::new(DocumentPreparations::default())),
        }
    }
    /// The immutable helper image shared with this owner's workers.
    pub fn helpers(&self) -> &Arc<lash_kernel_doc::BuildHelpers> {
        &self.helpers
    }

    pub(crate) fn prepared_from(&mut self, owner: &ProcessPreparations) {
        self.prepared = Arc::clone(&owner.prepared);
    }

    /// A current runtime binding sharing only this owner's pure content
    /// preparations. Reads and retention always use the supplied ports.
    pub fn bind(
        &self,
        store: Arc<dyn ModuleArtifactStore>,
        packages: Arc<dyn PackageStore>,
    ) -> Self {
        Self {
            store,
            packages,
            helpers: Arc::clone(&self.helpers),
            prepared: Arc::clone(&self.prepared),
        }
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
        self.admit_identified_document(claim, &document).await
    }

    /// Admit lash's immutable decoded content without losing its canonical
    /// bytes. Identity is already established; structural and type admission
    /// still run against the document's own definitions before publication.
    pub async fn admit_identified_document(
        &self,
        claim: &ReferrerClaim,
        document: &IdentifiedDocument,
    ) -> Result<DocumentId, EnginePortError> {
        let span = tracing::debug_span!("lash.artifacts.admit", document_id = %document.id(), document_bytes = document.body().len());
        async {
            self.check_identified_document(document)?;
            let artifact = document_artifact(&document.id());
            #[cfg(feature = "testing")]
            {
                lash_vm_client::packages::helper_document_work::published(document);
                lash_vm_client::packages::helper_document_work::staged(document);
            }
            async {
                self.store
                    .hold_module_artifacts(
                        claim,
                        &[ModuleArtifactHold::Publish {
                            module_ref: &artifact.artifact_ref,
                            body: ModuleArtifactBody::Document(document.body()),
                        }],
                    )
                    .await
                    .inspect_err(store_write_refused)
            }
            .instrument(
                tracing::debug_span!("lash.artifacts.store_write", document_id = %document.id()),
            )
            .await
            .map_err(artifact_error)?;
            Ok(document.id())
        }
        .instrument(span)
        .await
    }
    fn check_identified_document(
        &self,
        document: &IdentifiedDocument,
    ) -> Result<(), EnginePortError> {
        let slot = Self::document_slot(&self.prepared, &document.id(), document.body());
        slot.derived
            .get_or_init(|| {
                lash_kernel_check::derive_shared(document.shared(), self.helpers.as_ref())
                    .map(|_| ())
                    .map_err(|e| {
                        EnginePortError::Refused(AdmissionRefusal::InvalidDocument {
                            message: e.to_string(),
                        })
                    })
            })
            .clone()
            .inspect_err(|_| {
                tracing::debug!(
                    event = "artifact_refused",
                    phase = "admit",
                    refusal_kind = "admission_refused"
                );
            })?;
        // Derivation includes structural validation. This immutable value can
        // serve later reads only when their fetched bytes match in full.
        slot.decoded.get_or_init(|| Ok(document.clone()));
        slot.checked.get_or_init(|| Ok(document.clone()));
        Ok(())
    }

    /// Retain a freshly fulfilled cell and its accepted publication facts in
    /// one transaction. Preparation owns no storage lifetime; the execution
    /// claim arms its existing cleanup obligation at this atomic acquisition.
    pub async fn hold_cell_execution(
        &self,
        claim: &ReferrerClaim,
        document: &IdentifiedDocument,
        link: &Link,
        publications: &[PublicationRecord],
    ) -> Result<(), EnginePortError> {
        use lash_durable::packages::PackageHold;
        let record = LinkRecord {
            id: link.identity(),
            link: link.clone(),
        };
        let artifacts = [document_artifact(&document.id())];
        let mut holds = vec![
            PackageHold::Document {
                id: document.id(),
                body: document.body(),
            },
            PackageHold::Link {
                record: &record,
                artifacts: &artifacts,
            },
        ];
        holds.extend(publications.iter().map(PackageHold::Publication));
        #[cfg(feature = "testing")]
        {
            lash_vm_client::packages::helper_document_work::published(document);
            lash_vm_client::packages::helper_document_work::staged(document);
        }
        self.packages
            .acquire_batch(claim, &holds)
            .await
            .map_err(store_error)?;
        self.prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .linked
            .links
            .remove(&record.id);
        Ok(())
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
        let document = Self::check_current(&self.prepared, id, &bytes)?;
        #[cfg(feature = "testing")]
        lash_vm_client::packages::helper_document_work::got(&document);
        Ok(Some(document))
    }

    fn check_current(
        prepared: &Mutex<DocumentPreparations>,
        id: &DocumentId,
        bytes: &[u8],
    ) -> Result<IdentifiedDocument, EnginePortError> {
        let slot = Self::document_slot(prepared, id, bytes);
        // The slot is pure content, not a read or retention fact. Every caller
        // fetched current bytes before lookup; equality checks them all.
        let checked = slot
            .checked
            .get_or_init(|| Self::decode_current(prepared, id, bytes))
            .clone();
        if checked.is_err() {
            let mut owner = prepared
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            if let Some(at) = owner
                .documents
                .iter()
                .position(|known| Arc::ptr_eq(known, &slot))
            {
                owner.documents.remove(at);
                owner.document_bytes -= 2 * slot.bytes.len();
            }
        }
        checked
    }

    fn document_slot(
        prepared: &Mutex<DocumentPreparations>,
        id: &DocumentId,
        bytes: &[u8],
    ) -> Arc<PreparedDocument> {
        if bytes.len() > DOCUMENT_BYTES / 2 {
            return Arc::new(PreparedDocument {
                id: *id,
                bytes: bytes.into(),
                checked: OnceLock::new(),
                decoded: OnceLock::new(),
                derived: OnceLock::new(),
            });
        }
        let known = prepared
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .document(id, bytes);
        match known {
            Some(known) => known,
            None => {
                // Copy outside the inventory lock, then reclassify under the
                // same owner: concurrent first readers share one preparation.
                let candidate = Arc::new(PreparedDocument {
                    id: *id,
                    bytes: bytes.into(),
                    checked: OnceLock::new(),
                    decoded: OnceLock::new(),
                    derived: OnceLock::new(),
                });
                let mut owner = prepared
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                if let Some(known) = owner.document(id, bytes) {
                    known
                } else {
                    let size = 2 * bytes.len();
                    while owner.documents.len() >= DOCUMENTS
                        || owner.document_bytes + size > DOCUMENT_BYTES
                    {
                        if let Some(old) = owner.documents.pop_front() {
                            owner.document_bytes -= 2 * old.bytes.len();
                        }
                    }
                    owner.document_bytes += size;
                    owner.documents.push_back(Arc::clone(&candidate));
                    candidate
                }
            }
        }
    }

    fn decode_current(
        prepared: &Mutex<DocumentPreparations>,
        id: &DocumentId,
        bytes: &[u8],
    ) -> Result<IdentifiedDocument, EnginePortError> {
        let artifact = document_artifact(id);
        let slot = Self::document_slot(prepared, id, bytes);
        let document = slot
            .decoded
            .get_or_init(|| IdentifiedDocument::decode_standard(bytes))
            .clone()
            .map_err(|error| EnginePortError::Corrupt {
                artifact: artifact.clone(),
                message: error.to_string(),
            })?;
        if document.id() != *id {
            return Err(EnginePortError::Corrupt {
                artifact,
                message: "Document content hash differs".into(),
            });
        }
        #[cfg(test)]
        {
            prepared.lock().unwrap().validations += 1;
        }
        #[cfg(not(test))]
        let _ = prepared;
        lash_kernel_doc::validate_document_structure(&document).map_err(|e| {
            EnginePortError::Corrupt {
                artifact: document_artifact(id),
                message: e.to_string(),
            }
        })?;
        Ok(document)
    }
    pub async fn get(&self, id: &DocumentId) -> Result<Option<Document>, EnginePortError> {
        Ok(self
            .get_shared(id)
            .await?
            .map(IdentifiedDocument::into_document))
    }
    /// Full cold-load from immutable store records, including currently unused fills.
    pub async fn load_linked(&self, root: LinkId) -> Result<LinkedContent, EnginePortError> {
        self.load_linked_identified(root)
            .await
            .map(|(content, _)| content)
    }

    /// Load current linked content together with the document identities and
    /// canonical bodies validated by this same read. Consumers carry those
    /// facts intact instead of fetching the documents again.
    pub async fn load_linked_identified(
        &self,
        root: LinkId,
    ) -> Result<(LinkedContent, BTreeMap<DocumentId, IdentifiedDocument>), EnginePortError> {
        let mut content = LinkedContent::default();
        let mut identified = BTreeMap::new();
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
            if let std::collections::btree_map::Entry::Vacant(entry) =
                identified.entry(record.link.document)
            {
                let document = self
                    .get_shared(&record.link.document)
                    .await?
                    .ok_or_else(|| EnginePortError::Missing {
                        artifact: document_artifact(&record.link.document),
                    })?;
                entry.insert(document);
            }
            pending.extend(record.link.fills.values().filter_map(|f| match f {
                Fill::Export { link, .. } => Some(*link),
                _ => None,
            }));
            content.links.insert(id, record.link);
        }
        content.documents = identified
            .iter()
            .map(|(id, document)| (*id, Arc::clone(document.shared())))
            .collect();
        for link in content.links.values() {
            content.check_link(link).map_err(map_link_refusal)?;
        }
        content.retain_reach(root).map_err(map_link_refusal)?;
        Ok((content, identified))
    }
    async fn content_for(
        &self,
        document: &IdentifiedDocument,
        snapshot: &NamespaceSnapshot,
    ) -> Result<LinkedContent, EnginePortError> {
        let mut content = LinkedContent::default();
        content
            .documents
            .insert(document.id(), Arc::clone(document.shared()));
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
        // get() already checked these current bytes and their identity.
        content.documents.insert(link.document, Arc::new(document));
        for fill in link.fills.values() {
            if let Fill::Export { link, .. } = fill
                && !content.links.contains_key(link)
            {
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
    /// Publish an explicitly approved format successor with the predecessor's
    /// exact fills. This never fulfils against a current namespace.
    pub async fn hold_format_successor(
        &self,
        claim: &ReferrerClaim,
        predecessor: LinkId,
        document: &IdentifiedDocument,
    ) -> Result<LinkId, EnginePortError> {
        let content = self.load_linked(predecessor).await?;
        let old = content.link(predecessor).map_err(map_link_refusal)?;
        self.admit_identified_document(claim, document).await?;
        let next = Link {
            document: document.id(),
            fills: old.fills.clone(),
        };
        let id = next.identity();
        self.prepare_link(next).await?;
        self.hold(claim, &ContentRef::Link(id)).await?;
        Ok(id)
    }
    pub async fn fulfil(&self, request: &FulfilRequest) -> Result<Fulfilled, EnginePortError> {
        let document =
            self.get_shared(&request.document)
                .await?
                .ok_or_else(|| EnginePortError::Missing {
                    artifact: document_artifact(&request.document),
                })?;
        self.fulfil_checked(&document, request).await
    }

    /// Fulfil freshly lowered, identified content without publishing a
    /// document before the rest of its execution admission is ready.
    pub async fn fulfil_identified(
        &self,
        document: &IdentifiedDocument,
        request: &FulfilRequest,
    ) -> Result<Fulfilled, EnginePortError> {
        self.check_identified_document(document)?;
        if document.id() != request.document {
            return Err(EnginePortError::Corrupt {
                artifact: document_artifact(&request.document),
                message: "fulfilment document differs from its request".into(),
            });
        }
        self.fulfil_checked(document, request).await
    }

    async fn fulfil_checked(
        &self,
        document: &IdentifiedDocument,
        request: &FulfilRequest,
    ) -> Result<Fulfilled, EnginePortError> {
        let mut content = self.content_for(document, &request.snapshot).await?;
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
        let inspection = inspection(&content, id, &request.selector, &self.helpers)?;
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
        let span = tracing::debug_span!(
            "lash.artifacts.retain",
            publication_count = snapshot.advertisements.len()
        );
        artifact_phase("retain", span, async {
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
                        message:
                            "accepted publication is absent or differs from the retained record"
                                .into(),
                    });
                }
                self.packages
                    .acquire_publication(claim, &key)
                    .instrument(
                        tracing::debug_span!("lash.artifacts.package_hold", publication_key = ?key),
                    )
                    .await
                    .map_err(store_error)?;
            }
            Ok(())
        })
        .await
    }
    pub async fn inspect_link(
        &self,
        link: LinkId,
        selector: &ReachSelector,
    ) -> Result<LinkInspection, EnginePortError> {
        inspection(
            &self.load_linked(link).await?,
            link,
            selector,
            &self.helpers,
        )
    }
    /// Certifiable calls of the exact stored start. Merely held callable code
    /// remains available for recorded calls but cannot plan unrecorded reads.
    pub async fn invoked_operations(
        &self,
        start: &LinkedStart,
    ) -> Result<BTreeSet<lash_vocabulary::OperationUse>, EnginePortError> {
        let content = self.load_linked(start.link).await?;
        let reach = content
            .inspect_invocations(
                start.link,
                &ReachSelection::Start(start.selector.clone()),
                self.helpers.as_ref(),
            )
            .map_err(map_link_refusal)?;
        Ok(reach.operations.into_keys().collect())
    }

    /// Retain existing content by its stored pointer. Only a fresh, unpublished preparation may write a Link.
    pub async fn hold(
        &self,
        claim: &ReferrerClaim,
        reference: &ContentRef,
    ) -> Result<(), EnginePortError> {
        artifact_phase(
            "retain",
            tracing::debug_span!("lash.artifacts.retain"),
            async {
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
                        let mut checked = BTreeSet::new();
                        for member in record.package.members.values() {
                            if let MemberKind::Export { link, .. } = member.kind
                                && checked.insert(link)
                            {
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
                            // get() already checked these current bytes and identity.
                            content.documents.insert(link.document, Arc::new(document));
                            for fill in link.fills.values() {
                                if let Fill::Export { link, .. } = fill
                                    && !content.links.contains_key(link)
                                {
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
                                .instrument(tracing::debug_span!("lash.artifacts.store_write", link_id = %id))
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
            },
        )
        .await
    }
    pub async fn execution_document(
        &self,
        start: &LinkedStart,
    ) -> Result<lash_kernel_check::WorkflowDocument, EnginePortError> {
        let content = self.load_linked(start.link).await?;
        content
            .inspect(
                start.link,
                &ReachSelection::Start(start.selector.clone()),
                self.helpers.as_ref(),
            )
            .map_err(map_link_refusal)?;
        let document = content
            .document(content.link(start.link).map_err(map_link_refusal)?.document)
            .map_err(map_link_refusal)?
            .clone();
        lash_kernel_check::WorkflowDocument::entered(document, start.clone(), self.helpers.as_ref())
            .map_err(|e| {
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
    helpers: &lash_kernel_doc::BuildHelpers,
) -> Result<LinkInspection, EnginePortError> {
    let reach = content
        .inspect(root, &selection(selector), helpers)
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
        async {
            map.validate(document, source).map_err(|e| {
                tracing::debug!(
                    event = "artifact_refused",
                    phase = "check",
                    refusal_kind = "source_map"
                );
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
                .inspect_err(store_write_refused)
                .map_err(artifact_error)?;
            Ok(identity)
        }
        .instrument(tracing::debug_span!(
            "lash.artifacts.store_write",
            source_bytes = source.len()
        ))
        .await
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

#[cfg(test)]
mod residency_laws {
    use super::*;

    /// FIG-7135: cold cells and cuts validate each current document content
    /// once while resident; neither a supplied id nor an old read admits bytes.
    #[test]
    fn cold_document_validation_is_bounded_by_content_not_cells_or_cuts() {
        let prepared = Mutex::new(DocumentPreparations::default());
        let mut document = Document::new(lash_kernel_doc::NumberPolicy::BySpelling, vec![]);
        for index in 0..32 {
            let definition = lash_kernel_doc::parse_definition(&format!(
                "function retained_{index}() -> Null\nkernel 1\ncharge 1\nbody {{ return null }}"
            ))
            .unwrap();
            document.functions.insert(definition.identity(), definition);
        }
        let document = IdentifiedDocument::new(document);
        for _cell in 0..4 {
            for _cut in 0..3 {
                let current_bytes = document.body().to_vec();
                let checked =
                    KernelDocuments::check_current(&prepared, &document.id(), &current_bytes)
                        .expect("current valid bytes");
                assert_eq!(checked.id(), document.id());
            }
        }
        assert_eq!(
            prepared.lock().unwrap().validations,
            1,
            "cold cells and cuts repeated structural document validation"
        );
        let changed =
            IdentifiedDocument::new(Document::new(lash_kernel_doc::NumberPolicy::Float, vec![]));
        assert!(
            matches!(KernelDocuments::check_current(&prepared, &document.id(), changed.body()),
            Err(EnginePortError::Corrupt { message, .. }) if message == "Document content hash differs")
        );
        assert!(
            matches!(KernelDocuments::check_current(&prepared, &document.id(), &[255]),
            Err(EnginePortError::Corrupt { message, .. }) if message != "Document content hash differs")
        );
        let invalid = IdentifiedDocument::new(
            lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { break }").unwrap(),
        );
        assert!(matches!(
            KernelDocuments::check_current(&prepared, &invalid.id(), invalid.body()),
            Err(EnginePortError::Corrupt { .. })
        ));
    }

    /// FIG-7135: racing cold readers share validation; retained preparation
    /// remains bounded and an evicted identity earns validation again.
    #[test]
    fn cold_document_residency_serializes_races_and_revalidates_after_eviction() {
        let prepared = Arc::new(Mutex::new(DocumentPreparations::default()));
        let document = IdentifiedDocument::new(Document::new(
            lash_kernel_doc::NumberPolicy::BySpelling,
            vec![],
        ));
        let barrier = Arc::new(std::sync::Barrier::new(4));
        let checked = std::thread::scope(|scope| {
            let readers: Vec<_> = (0..4)
                .map(|_| {
                    let prepared = Arc::clone(&prepared);
                    let barrier = Arc::clone(&barrier);
                    let bytes = document.body().to_vec();
                    let id = document.id();
                    scope.spawn(move || {
                        barrier.wait();
                        KernelDocuments::check_current(&prepared, &id, &bytes).unwrap()
                    })
                })
                .collect();
            readers
                .into_iter()
                .map(|reader| reader.join().unwrap())
                .collect::<Vec<_>>()
        });
        assert_eq!(prepared.lock().unwrap().validations, 1);
        for read in &checked {
            assert!(Arc::ptr_eq(checked[0].shared(), read.shared()));
        }
        for index in 0..DOCUMENTS {
            let mut other = (*document.document()).clone();
            other
                .private_bindings
                .insert(Name::new(format!("resident_{index}")));
            let other = IdentifiedDocument::new(other);
            KernelDocuments::check_current(&prepared, &other.id(), other.body()).unwrap();
            let owner = prepared.lock().unwrap();
            assert!(owner.documents.len() <= DOCUMENTS);
            assert!(owner.document_bytes <= DOCUMENT_BYTES);
        }
        KernelDocuments::check_current(&prepared, &document.id(), document.body()).unwrap();
        assert_eq!(prepared.lock().unwrap().validations, DOCUMENTS + 2);
    }
}

#[cfg(test)]
mod store_cause_laws {
    use super::*;
    use lash_core::store::{StoreFault, StoreRefusal};
    use lash_execution_contracts::PluginError;

    fn assert_cause(error: EnginePortError, expected: PluginError) {
        let decoded: EnginePortError =
            serde_json::from_slice(&serde_json::to_vec(&error).unwrap()).unwrap();
        match (PluginError::from(decoded), expected) {
            (
                PluginError::StoreUnavailable { fault: actual },
                PluginError::StoreUnavailable { fault: expected },
            ) => assert_eq!(actual, expected),
            (PluginError::StoreRefusal(actual), PluginError::StoreRefusal(expected)) => {
                assert_eq!(actual, expected)
            }
            (actual, expected) => panic!("store cause changed: {actual:?}, expected {expected:?}"),
        }
    }

    /// FIG-7296: document and package store faults keep their typed fields
    /// through the serializable engine port and the outer plugin boundary.
    #[test]
    fn store_causes_survive_document_and_plugin_ports() {
        for fault in [
            StoreFault::Contended,
            StoreFault::StorageFailure {
                backend: "sqlite".into(),
                message: "read interrupted".into(),
            },
            StoreFault::MigrationOpenElsewhere {
                database: "documents".into(),
                location: "documents.sqlite".into(),
            },
            StoreFault::Backend {
                message: "read interrupted".into(),
            },
        ] {
            assert_cause(
                artifact_error(ArtifactStoreError::StoreFault(fault.clone())),
                PluginError::StoreUnavailable { fault },
            );
        }
        assert_cause(
            artifact_error(ArtifactStoreError::Backend("read interrupted".into())),
            PluginError::StoreUnavailable {
                fault: StoreFault::Backend {
                    message: "read interrupted".into(),
                },
            },
        );
        for (kind, fault) in [
            (
                lash_durable::StoreFailureKind::Contended,
                StoreFault::Contended,
            ),
            (
                lash_durable::StoreFailureKind::Unavailable,
                StoreFault::Backend {
                    message: "read interrupted".into(),
                },
            ),
        ] {
            assert_cause(
                store_error(lash_durable::DurableError::Store(
                    lash_durable::StoreFailure {
                        kind,
                        message: "read interrupted".into(),
                    },
                )),
                PluginError::StoreUnavailable { fault },
            );
        }
        let refusal = StoreRefusal::SessionStateVersionNewerThanRuntime {
            found: 2,
            current: 1,
        };
        assert_cause(
            artifact_error(ArtifactStoreError::StoreRefusal(refusal.clone())),
            PluginError::StoreRefusal(refusal.clone()),
        );
        assert_cause(
            store_error(lash_durable::DurableError::StoreRefused(refusal.clone())),
            PluginError::StoreRefusal(refusal),
        );
    }
}
