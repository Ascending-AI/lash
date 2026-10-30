//! The pool's immutable library and document image. Per-run content names this exact owner.
use crate::PoolError;
use lash_kernel_doc::{BuildHelpers, FunctionRegistry};
use lash_kernel_doc::{Document, IdentifiedDocument};
use lash_kernel_value::names::DocumentId;
use lash_kernel_vm::PreparedLibrary;
use lash_protocol_types::engine_fault::{PayloadKind, WorkerLibraryRefusal};
use lash_vm_protocol::{EncodedPayload, FrameCodec, PoolStarted};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};
use {
    lash_kernel_value::tokens::TokenCodec as _, lash_kernel_value::tokens::TokenReader,
    lash_kernel_value::tokens::TokenWriter,
};

#[derive(Clone, Debug)]
pub struct WorkerLibrary {
    frame: Arc<[u8]>,
    live: Arc<OnceLock<&'static [u8]>>,
    registry: Arc<FunctionRegistry>,
    roots: std::collections::BTreeSet<lash_kernel_value::names::FunctionId>,
    documents: Arc<BTreeMap<DocumentId, IdentifiedDocument>>,
    helpers: Arc<BuildHelpers>,
}

impl WorkerLibrary {
    /// The process's one immutable build image, shared across owners and pools.
    pub fn built() -> Result<Arc<Self>, lash_kernel_releases::LibraryError> {
        static BUILT: OnceLock<Result<Arc<WorkerLibrary>, lash_kernel_releases::LibraryError>> =
            OnceLock::new();
        BUILT
            .get_or_init(|| {
                let release = lash_kernel_releases::helper_release()?;
                Self::with_helpers(
                    &release.functions,
                    release.resolver_roots(),
                    &BTreeMap::new(),
                    Arc::clone(&release.helpers),
                )
                .map(Arc::new)
                .map_err(lash_kernel_releases::LibraryError::new)
            })
            .clone()
    }

    pub fn new(
        functions: &FunctionRegistry,
        roots: impl IntoIterator<Item = lash_kernel_value::names::FunctionId>,
        documents: &BTreeMap<DocumentId, Document>,
    ) -> Result<Self, WorkerLibraryRefusal> {
        let release = lash_kernel_releases::helper_release().map_err(malformed)?;
        let members = release
            .helpers
            .members()
            .filter(|id| functions.get(id).is_some())
            .copied();
        let mut helpers = BuildHelpers::new(functions.clone(), members).map_err(malformed)?;
        if helpers.members().eq(release.helpers.members()) {
            helpers = helpers
                .with_library_identities(release.helpers.library_identities().iter().copied())
                .with_claim_generations(
                    release.helpers.current_identity(),
                    release.helpers.claim_generations().clone(),
                )
                .map_err(malformed)?;
        }
        Self::with_helpers(functions, roots, documents, Arc::new(helpers))
    }

    /// Install an exact closed helper image independently of host-authored Documents.
    pub fn with_helpers(
        functions: &FunctionRegistry,
        roots: impl IntoIterator<Item = lash_kernel_value::names::FunctionId>,
        documents: &BTreeMap<DocumentId, Document>,
        helpers: Arc<BuildHelpers>,
    ) -> Result<Self, WorkerLibraryRefusal> {
        // A two-version build carries both exact helper declarations. Native
        // aliases project the migration's identity pairs into the existing
        // frame binding mechanism; source resolution still selects the
        // dialect's original roots.
        #[cfg(feature = "synthetic-next")]
        let (functions, helpers, native_roots) = {
            let mut registry = functions.clone();
            let migration =
                lash_kernel_migrate::migration_from(lash_kernel_doc::KernelVersion::One)
                    .ok_or_else(|| malformed("the successor has no helper migration"))?;
            let pairs = lash_kernel_migrate::migrate_registry(&mut registry, migration)
                .map_err(malformed)?;
            let members = || {
                helpers.members().copied().chain(
                    pairs
                        .iter()
                        .filter_map(|(from, to)| helpers.contains(from).then_some(*to)),
                )
            };
            let current = lash_kernel_value::HelperLibraryId::of_members(members());
            let helpers = Arc::new(
                BuildHelpers::new(registry.clone(), members())
                    .map_err(malformed)?
                    .with_library_identities(helpers.library_identities().iter().copied())
                    .with_claim_generations(current, helpers.claim_generations().clone())
                    .map_err(malformed)?,
            );
            let native_roots: BTreeMap<_, _> =
                pairs.into_iter().map(|(from, to)| (to, from)).collect();
            (registry, helpers, native_roots)
        };
        #[cfg(feature = "synthetic-next")]
        let functions = &functions;
        #[cfg(not(feature = "synthetic-next"))]
        let native_roots = BTreeMap::new();
        for id in helpers.members() {
            if functions.get(id).is_none() {
                return Err(malformed(format!(
                    "helper {id} is absent from worker image"
                )));
            }
        }
        let mut metadata = TokenWriter::new();
        metadata.seq(5);
        metadata.bytes(helpers.identity().as_bytes());
        helpers
            .members()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            .encode(&mut metadata);
        helpers.library_identities().encode(&mut metadata);
        helpers.current_identity().encode(&mut metadata);
        helpers.claim_generations().encode(&mut metadata);
        let metadata = metadata.into_bytes();
        let roots: std::collections::BTreeSet<_> = roots.into_iter().collect();
        let root_bytes = lash_kernel_value::tokens::encode_body(&roots);
        let documents: BTreeMap<_, _> = documents
            .iter()
            .map(|(id, document)| {
                let document = IdentifiedDocument::new(document.clone());
                if document.id() != *id {
                    return Err(malformed("image document identity differs"));
                }
                Ok((*id, document))
            })
            .collect::<Result<_, _>>()?;
        let mut content = TokenWriter::new();
        content.seq(documents.len() as u64);
        for (id, document) in &documents {
            content.seq(2);
            id.encode(&mut content);
            content.bytes(document.body());
        }
        let content = content.into_bytes();
        let image = functions
            .to_image_with_census(&|definition| lash_kernel_vm::body_census(definition, functions));
        let index = PreparedLibrary::frame_index(functions);
        let positions = functions
            .frame_positions(&native_roots)
            .map_err(malformed)?;
        let mut frame = Vec::new();
        for part in [&image, &index, &positions, &root_bytes, &content, &metadata] {
            let length: u32 = part.len().try_into().map_err(malformed)?;
            frame.extend_from_slice(&length.to_le_bytes());
            frame.extend_from_slice(part);
        }
        Ok(Self {
            frame: frame.into(),
            live: Arc::new(OnceLock::new()),
            registry: Arc::new(functions.clone()),
            roots,
            documents: Arc::new(documents),
            helpers,
        })
    }
    pub fn roots(&self) -> &std::collections::BTreeSet<lash_kernel_value::names::FunctionId> {
        &self.roots
    }
    pub fn helpers(&self) -> &Arc<BuildHelpers> {
        &self.helpers
    }
    pub fn registry(&self) -> &FunctionRegistry {
        &self.registry
    }
    pub fn frame(&self) -> Arc<[u8]> {
        self.frame.clone()
    }
    pub fn from_frame(frame: Arc<[u8]>) -> Result<Self, WorkerLibraryRefusal> {
        sections(&frame)?;
        let owner: &'static [u8] = Box::leak(frame.to_vec().into_boxed_slice());
        let [image, _, _, roots, content, metadata] = sections(owner)?;
        let documents = read_documents(content)?;
        let roots = lash_kernel_value::tokens::decode_body(roots).map_err(malformed)?;
        let registry = Arc::new(FunctionRegistry::over_frame(image).map_err(malformed)?);
        let mut reader = TokenReader::new(metadata);
        reader.expect_struct(5).map_err(malformed)?;
        let identity = lash_kernel_value::HelperLibraryId::from_bytes(
            reader
                .bytes()
                .map_err(malformed)?
                .try_into()
                .map_err(malformed)?,
        );
        let members =
            std::collections::BTreeSet::<lash_kernel_value::FunctionId>::decode(&mut reader)
                .map_err(malformed)?;
        let libraries =
            std::collections::BTreeSet::<lash_kernel_value::HelperLibraryId>::decode(&mut reader)
                .map_err(malformed)?;
        let current = lash_kernel_value::HelperLibraryId::decode(&mut reader).map_err(malformed)?;
        let generations = BTreeMap::decode(&mut reader).map_err(malformed)?;
        reader.finish().map_err(malformed)?;
        let helpers = Arc::new(
            BuildHelpers::new(registry.as_ref().clone(), members)
                .map_err(malformed)?
                .with_library_identities(libraries)
                .with_claim_generations(current, generations)
                .map_err(malformed)?,
        );
        if helpers.identity() != identity {
            return Err(malformed(
                "helper library identity differs from image membership",
            ));
        }
        Ok(Self {
            frame,
            live: Arc::new(OnceLock::from(owner)),
            registry,
            roots,
            documents: Arc::new(documents),
            helpers,
        })
    }
    pub fn indexes(&self) -> Result<[&'static [u8]; 4], WorkerLibraryRefusal> {
        let bytes = self
            .live
            .get_or_init(|| Box::leak(self.frame.to_vec().into_boxed_slice()));
        let [image, index, positions, roots, _, _] = sections(bytes)?;
        Ok([image, index, positions, roots])
    }

    /// Exact immutable code owned by this worker image.
    fn document(&self, id: &DocumentId) -> Option<&IdentifiedDocument> {
        self.documents.get(id)
    }

    /// Project a received document against this exact worker image before
    /// counting its per-run transport reservation.
    pub(crate) fn prepare_document(
        &self,
        document: &mut lash_vm_protocol::message::StoredDocument,
    ) -> Result<(), PoolError> {
        use lash_vm_protocol::message::StoredDocument;
        let id = document.identity();
        match document {
            StoredDocument::Supplied { body, .. } => {
                if let Some(held) = self.document(&id) {
                    if held.body() != body.0.as_slice() {
                        return Err(PoolError::payload(
                            PayloadKind::Start,
                            "image document bytes differ from their identity",
                        ));
                    }
                    *document = StoredDocument::Image { document: id };
                }
            }
            StoredDocument::Cached { .. } => {}
            StoredDocument::Image { .. } => {
                if self.document(&id).is_none() {
                    return Err(image_document_missing(id));
                }
            }
        }
        Ok(())
    }

    pub(crate) fn prepare_start(
        &self,
        start: &mut lash_vm_protocol::Start,
    ) -> Result<(), PoolError> {
        self.prepare_document(&mut start.document)?;
        for document in &mut start.documents {
            self.prepare_document(document)?;
        }
        // The primary document already crosses in its own field. Equality is
        // over the actual supplied bytes or the exact image-owned identity.
        start
            .documents
            .retain(|document| document != &start.document);
        Ok(())
    }
}
mod runtime_ops {
    pub trait Sealed {}
    impl Sealed for super::WorkerLibrary {}
}

/// Sealed transport support for the worker implementation, outside the host API.
/// Hosts supply image content through `WorkerLibrary::new`.
pub trait WorkerLibraryRuntimeOps: runtime_ops::Sealed {
    fn resolve_document(
        &self,
        document: &lash_vm_protocol::message::StoredDocument,
        limits: lash_kernel_value::data_limits::KernelDataLimits,
    ) -> Result<IdentifiedDocument, PoolError>;
}
// Cross-crate runtime support, as with WorkerPoolRuntimeOps (ADR 0051).
#[doc(hidden)]
impl WorkerLibraryRuntimeOps for WorkerLibrary {
    /// Resolve exact content from this image, or decode the bytes supplied by
    /// this execution. A narrower receiving decoder still enforces its limits.
    fn resolve_document(
        &self,
        document: &lash_vm_protocol::message::StoredDocument,
        limits: lash_kernel_value::data_limits::KernelDataLimits,
    ) -> Result<IdentifiedDocument, PoolError> {
        use lash_vm_protocol::message::StoredDocument;
        let invalid = |error: &dyn std::fmt::Display| {
            PoolError::refused(lash_protocol_types::engine_fault::RunRefusal::Undecodable {
                input: lash_protocol_types::engine_fault::RunInput::Document,
                detail: lash_protocol_types::engine_fault::Detail::new(error),
            })
        };
        let loaded = match document {
            StoredDocument::Cached { document } => {
                return Err(PoolError::refused(
                    lash_protocol_types::engine_fault::RunRefusal::DocumentNotSupplied {
                        document: *document,
                    },
                ));
            }
            StoredDocument::Image { document } => {
                let held = self
                    .document(document)
                    .ok_or_else(|| image_document_missing(*document))?;
                if limits == lash_kernel_value::data_limits::KernelDataLimits::standard() {
                    held.clone()
                } else {
                    IdentifiedDocument::decode(held.body(), limits)
                        .map_err(|error| invalid(&error))?
                }
            }
            StoredDocument::Supplied { body, .. } => {
                IdentifiedDocument::decode(&body.0, limits).map_err(|error| invalid(&error))?
            }
        };
        if loaded.id() != document.identity() {
            return Err(invalid(&"document bytes differ from their identity"));
        }
        Ok(loaded)
    }
}
fn malformed(error: impl std::fmt::Display) -> WorkerLibraryRefusal {
    WorkerLibraryRefusal::Malformed {
        problem: error.to_string(),
    }
}
fn sections(bytes: &[u8]) -> Result<[&[u8]; 6], WorkerLibraryRefusal> {
    let mut parts = [&[][..]; 6];
    let mut at = 0usize;
    for part in &mut parts {
        let word: [u8; 4] = bytes
            .get(at..at + 4)
            .ok_or_else(|| malformed("library section header"))?
            .try_into()
            .map_err(malformed)?;
        at += 4;
        let end = at
            .checked_add(u32::from_le_bytes(word) as usize)
            .ok_or_else(|| malformed("library section overflow"))?;
        *part = bytes
            .get(at..end)
            .ok_or_else(|| malformed("library section is truncated"))?;
        at = end;
    }
    if at != bytes.len() {
        return Err(malformed("library frame has trailing bytes"));
    }
    Ok(parts)
}
pub fn read(frame: Arc<[u8]>) -> Result<WorkerLibrary, PoolError> {
    WorkerLibrary::from_frame(frame).map_err(PoolError::Library)
}
pub fn result(result: Result<(), WorkerLibraryRefusal>) -> Result<PoolStarted, PoolError> {
    Ok(PoolStarted {
        result: EncodedPayload(
            rmp_serde::to_vec_named(&lash_kernel_value::json_number::Native(&result))
                .map_err(|error| PoolError::payload(PayloadKind::Bootstrap, error))?,
        ),
    })
}
pub(crate) fn installed(reply: PoolStarted, codec: &FrameCodec) -> Result<(), PoolError> {
    codec.check_payload(&reply.result.0)?;
    let result: Result<(), WorkerLibraryRefusal> = rmp_serde::from_slice(&reply.result.0)
        .map_err(|error| PoolError::payload(PayloadKind::Bootstrap, error))?;
    result.map_err(PoolError::Library)
}

fn read_documents(
    content: &[u8],
) -> Result<BTreeMap<DocumentId, IdentifiedDocument>, WorkerLibraryRefusal> {
    let mut reader = TokenReader::new(content);
    let count = reader.seq().map_err(malformed)?;
    let mut documents = BTreeMap::new();
    let mut previous = None;
    for _ in 0..count {
        reader.expect_struct(2).map_err(malformed)?;
        let id = DocumentId::decode(&mut reader).map_err(malformed)?;
        if previous.is_some_and(|prior| prior >= id) {
            return Err(malformed("image documents are not ascending"));
        }
        previous = Some(id);
        let body = reader.bytes().map_err(malformed)?;
        let document = IdentifiedDocument::decode_standard(body).map_err(malformed)?;
        if document.id() != id {
            return Err(malformed("image document identity differs"));
        }
        documents.insert(id, document);
    }
    reader.finish().map_err(malformed)?;
    Ok(documents)
}

fn image_document_missing(document: DocumentId) -> PoolError {
    PoolError::refused(
        lash_protocol_types::engine_fault::RunRefusal::ImageDocumentMissing { document },
    )
}

#[cfg(test)]
mod document_work_laws {
    use super::*;
    use lash_vm_protocol::{EncodedPayload, message::StoredDocument};

    /// FIG-7239: pool-start plan refusals retain identity and name at the host.
    #[test]
    fn a_missing_helper_plan_stays_typed_across_pool_start() {
        let refusal = WorkerLibraryRefusal::PlanMissing {
            function: lash_kernel_value::FunctionId::from_bytes([7; 32]),
            name: lash_kernel_value::FunctionName::new("ts.lt").expect("helper name"),
        };
        let reply = result(Err(refusal.clone())).expect("pool-start encoding");
        let codec = FrameCodec::new(lash_vm_protocol::DecodeLimits::standard());
        assert_eq!(installed(reply, &codec), Err(PoolError::Library(refusal)));
    }

    /// FIG-7135: a run's image-owned code crosses by identity, regardless
    /// of unrelated bytes retained in that immutable document.
    #[test]
    fn start_document_bytes_are_independent_of_image_document_size() {
        let mut sizes = Vec::new();
        for size in [1024, 1024 * 1024] {
            let document = lash_kernel_doc::parse_document(&format!(
                "kernel 1\nnumbers float\nmain {{ return \"{}\" }}",
                "x".repeat(size)
            ))
            .expect("valid image document");
            let id = document.identity();
            let mut supplied = StoredDocument::Supplied {
                document: id,
                body: EncodedPayload(document.canonical_bytes()),
            };
            let library = WorkerLibrary::new(
                &FunctionRegistry::declarations(),
                [],
                &[(id, document)].into(),
            )
            .expect("image");
            let library = WorkerLibrary::from_frame(library.frame()).expect("received image");
            let mut changed = supplied.clone();
            if let StoredDocument::Supplied { body, .. } = &mut changed {
                body.0.push(0);
            }
            assert!(
                library.prepare_document(&mut changed).is_err(),
                "changed bytes cannot borrow an image identity"
            );
            library
                .prepare_document(&mut supplied)
                .expect("prepare image-owned content");
            let resolved = library
                .resolve_document(&supplied, Default::default())
                .expect("exact image document");
            assert_eq!(resolved.id(), id);
            let empty =
                WorkerLibrary::new(&FunctionRegistry::declarations(), [], &Default::default())
                    .expect("empty image");
            assert_eq!(
                empty
                    .resolve_document(&supplied, Default::default())
                    .expect_err("no by-value fallback"),
                image_document_missing(id)
            );
            let narrow = lash_kernel_value::data_limits::KernelDataLimits::new(0, 352, 0, 512, 128)
                .expect("narrow limits");
            assert!(
                library.resolve_document(&supplied, narrow).is_err(),
                "image ownership cannot bypass receiving limits"
            );
            sizes.push(
                rmp_serde::to_vec(&supplied)
                    .expect("start document encoding")
                    .len(),
            );
        }
        assert_eq!(
            sizes[0], sizes[1],
            "per-run document bytes grew with image-owned host content"
        );
    }
}

/// Scoped parent-side attribution of forbidden helper Document work. Only
/// scalar counts are retained; the probe owns no Documents or execution cache.
#[cfg(any(test, feature = "testing"))]
pub mod helper_document_work {
    use lash_kernel_doc::Document;
    use std::collections::BTreeSet;
    use std::sync::{Mutex, PoisonError};
    use {lash_kernel_value::DocumentId, lash_kernel_value::FunctionId};

    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct Work {
        pub publications: u64,
        pub gets: u64,
        pub stages: u64,
        pub start_documents: u64,
        pub start_bytes: u64,
        pub start_instances: u64,
        pub start_definitions: u64,
    }
    static WORK: Mutex<Option<Work>> = Mutex::new(None);

    /// One exact-law window, spanning its async and delegated blocking work.
    pub struct Window;
    impl Window {
        pub fn begin() -> Self {
            let mut work = WORK.lock().unwrap_or_else(PoisonError::into_inner);
            assert!(
                work.is_none(),
                "a helper Document work window is already active"
            );
            *work = Some(Work::default());
            Self
        }
        pub fn snapshot(&self) -> Work {
            WORK.lock()
                .unwrap_or_else(PoisonError::into_inner)
                .unwrap_or_default()
        }
    }
    impl Drop for Window {
        fn drop(&mut self) {
            *WORK.lock().unwrap_or_else(PoisonError::into_inner) = None;
        }
    }
    fn active() -> bool {
        WORK.lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_some()
    }
    fn note(change: impl FnOnce(&mut Work)) {
        if let Some(work) = WORK.lock().unwrap_or_else(PoisonError::into_inner).as_mut() {
            change(work);
        }
    }
    fn helper(function: &FunctionId) -> bool {
        lash_kernel_releases::helper_release().is_ok_and(|release| {
            release
                .functions
                .get(function)
                .is_some_and(|registered| registered.has_body())
        })
    }
    pub fn contains_helper_body(document: &Document) -> bool {
        active()
            && document
                .functions
                .iter()
                .any(|(id, definition)| definition.body().is_some() && helper(id))
    }
    pub fn published(document: &Document) {
        if contains_helper_body(document) {
            note(|work| work.publications += 1);
        }
    }
    pub fn staged(document: &Document) {
        if contains_helper_body(document) {
            note(|work| work.stages += 1);
        }
    }
    pub fn got(document: &Document) {
        if contains_helper_body(document) {
            note(|work| work.gets += 1);
        }
    }
    pub(crate) fn start(
        start: &lash_vm_protocol::Start,
        documents: &BTreeSet<DocumentId>,
    ) -> Result<(), crate::PoolError> {
        if !active() {
            return Ok(());
        }
        let mut found = Work::default();
        for document in std::iter::once(&start.document).chain(&start.documents) {
            if documents.contains(&document.identity()) {
                found.start_documents += 1;
                if let lash_vm_protocol::message::StoredDocument::Supplied { body, .. } = document {
                    found.start_bytes += body.0.len() as u64;
                }
            }
        }
        let instances: Vec<lash_kernel_vm::interface::PreparedInstance> = crate::wire::decode(
            lash_protocol_types::engine_fault::PayloadKind::Start,
            &start.instances,
        )?;
        found.start_instances = instances
            .iter()
            .filter(|instance| documents.contains(&instance.document))
            .count() as u64;
        found.start_definitions = start
            .definitions
            .iter()
            .filter(|definition| helper(&FunctionId::from_bytes(definition.function)))
            .count() as u64;
        note(|work| {
            work.start_documents += found.start_documents;
            work.start_bytes += found.start_bytes;
            work.start_instances += found.start_instances;
            work.start_definitions += found.start_definitions;
        });
        Ok(())
    }
}
