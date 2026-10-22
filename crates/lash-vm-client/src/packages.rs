//! The pool's immutable library and document image. Per-run content names this exact owner.
use crate::PoolError;
use lash_kernel_doc::FunctionRegistry;
use lash_kernel_doc::{Document, IdentifiedDocument};
use lash_kernel_vm::PreparedLibrary;
use lash_protocol_types::engine_fault::{PayloadKind, WorkerLibraryRefusal};
use lash_vm_protocol::{EncodedPayload, FrameCodec, PoolStarted};
use lash_vocabulary::names::DocumentId;
use lash_vocabulary::tokens::{TokenCodec as _, TokenReader, TokenWriter};
use std::collections::BTreeMap;
use std::sync::{Arc, OnceLock};

#[derive(Clone, Debug)]
pub struct WorkerLibrary {
    frame: Arc<[u8]>,
    live: Arc<OnceLock<&'static [u8]>>,
    registry: Arc<FunctionRegistry>,
    roots: std::collections::BTreeSet<lash_vocabulary::names::FunctionId>,
    documents: Arc<BTreeMap<DocumentId, IdentifiedDocument>>,
}

impl WorkerLibrary {
    pub fn new(
        functions: &FunctionRegistry,
        roots: impl IntoIterator<Item = lash_vocabulary::names::FunctionId>,
        documents: &BTreeMap<DocumentId, Document>,
    ) -> Result<Self, WorkerLibraryRefusal> {
        let roots: std::collections::BTreeSet<_> = roots.into_iter().collect();
        let root_bytes = lash_vocabulary::tokens::encode_body(&roots);
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
            .frame_positions(&Default::default())
            .map_err(malformed)?;
        let mut frame = Vec::new();
        for part in [&image, &index, &positions, &root_bytes, &content] {
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
        })
    }
    pub fn roots(&self) -> &std::collections::BTreeSet<lash_vocabulary::names::FunctionId> {
        &self.roots
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
        let [image, _, _, roots, content] = sections(owner)?;
        let documents = read_documents(content)?;
        let roots = lash_vocabulary::tokens::decode_body(roots).map_err(malformed)?;
        let registry = Arc::new(FunctionRegistry::over_frame(image).map_err(malformed)?);
        Ok(Self {
            frame,
            live: Arc::new(OnceLock::from(owner)),
            registry,
            roots,
            documents: Arc::new(documents),
        })
    }
    pub fn indexes(&self) -> Result<[&'static [u8]; 4], WorkerLibraryRefusal> {
        let bytes = self
            .live
            .get_or_init(|| Box::leak(self.frame.to_vec().into_boxed_slice()));
        let [image, index, positions, roots, _] = sections(bytes)?;
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
/// Runtime transport operations used by the worker implementation.
/// The host supplies image content through `WorkerLibrary::new`.
pub trait WorkerLibraryRuntimeOps {
    fn resolve_document(
        &self,
        document: &lash_vm_protocol::message::StoredDocument,
        limits: lash_vocabulary::data_limits::KernelDataLimits,
    ) -> Result<IdentifiedDocument, PoolError>;
}
impl WorkerLibraryRuntimeOps for WorkerLibrary {
    /// Resolve exact content from this image, or decode the bytes supplied by
    /// this execution. A narrower receiving decoder still enforces its limits.
    fn resolve_document(
        &self,
        document: &lash_vm_protocol::message::StoredDocument,
        limits: lash_vocabulary::data_limits::KernelDataLimits,
    ) -> Result<IdentifiedDocument, PoolError> {
        use lash_vm_protocol::message::StoredDocument;
        let invalid = |error: &dyn std::fmt::Display| {
            PoolError::refused(lash_protocol_types::engine_fault::RunRefusal::Undecodable {
                input: lash_protocol_types::engine_fault::RunInput::Document,
                detail: lash_protocol_types::engine_fault::Detail::new(error),
            })
        };
        let loaded = match document {
            StoredDocument::Image { document } => {
                let held = self
                    .document(document)
                    .ok_or_else(|| image_document_missing(*document))?;
                if limits == lash_vocabulary::data_limits::KernelDataLimits::standard() {
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
fn sections(bytes: &[u8]) -> Result<[&[u8]; 5], WorkerLibraryRefusal> {
    let mut parts = [&[][..]; 5];
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
            rmp_serde::to_vec_named(&result)
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

#[cfg(test)]
#[expect(
    clippy::expect_used,
    reason = "the work law names broken fixture assumptions"
)]
mod document_work_laws {
    use super::*;
    use lash_vm_protocol::{EncodedPayload, message::StoredDocument};

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
            let narrow = lash_vocabulary::data_limits::KernelDataLimits::new(0, 352, 0, 512, 128)
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
            "per-run document bytes grew with image-owned helper content"
        );
    }
}

fn image_document_missing(document: DocumentId) -> PoolError {
    PoolError::refused(
        lash_protocol_types::engine_fault::RunRefusal::ImageDocumentMissing { document },
    )
}
