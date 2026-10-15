//! The admitted documents processes run, in the store set's module port.
//!
//! A document is stored once, as its canonical JSON, under its identity:
//! the hash of its content, kernel version and manifest included
//! (`K-DOC-003`). [`KernelDocuments::publish`] is the one writer: beside the
//! document it holds the definition of every library function the
//! document lists, each under its own identity, so a reader draws the
//! document from what it retains and never from its own build's registry
//! (FIG-6379). Equal helper sets are stored once, whichever documents list
//! them.
//!
//! A source map never changes behaviour and is not part of a document. A
//! host that publishes one with a workflow has it stored beside the source
//! it maps, under the digest of its canonical bytes, and keeps that id
//! itself. A document a migration or a successor carry rewrote records the
//! correspondence it was rewritten by, so a host can carry a map it holds
//! for the earlier document onto it.

use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use lash_core::{ArtifactName, ArtifactStoreError, ModuleArtifactStore, ReferrerClaim};
use lash_kernel_check::{WorkflowDocument, WorkflowDocumentError};
use lash_kernel_doc::{
    Correspondence, Document, DocumentId, FunctionDeclarations, FunctionDefinition, FunctionId,
    SourceMap, SourceMapId, WorkflowDocumentRef,
};

/// The module artifact a document is stored under.
pub(crate) fn document_artifact(document: &DocumentId) -> ArtifactName {
    ArtifactName {
        store: lash_core::ArtifactStoreId::KernelDocument,
        artifact_ref: document.to_string(),
    }
}

/// The module artifact a library function's definition is stored under.
pub(crate) fn function_artifact(function: &FunctionId) -> ArtifactName {
    ArtifactName {
        store: lash_core::ArtifactStoreId::KernelDocument,
        artifact_ref: format!("function:{function}"),
    }
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

fn rewrite_artifact(document: &DocumentId) -> ArtifactName {
    ArtifactName {
        store: lash_core::ArtifactStoreId::KernelDocument,
        artifact_ref: format!("rewrite:{document}"),
    }
}

/// An artifact a read needs that is not retained as it should be.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Unretained {
    /// Nothing retains it any more.
    Missing { artifact: ArtifactName },
    /// It is retained with bytes that are not the artifact its name says.
    Corrupt {
        artifact: ArtifactName,
        message: String,
    },
}

/// A document an execution names, read from what the store retains.
#[derive(Clone, Debug, PartialEq)]
pub struct ExecutionDocument {
    /// The document entered where the reference says, held with the
    /// declarations it was stored with.
    pub document: WorkflowDocument,
    /// The document this one was rewritten from by a kernel migration or a
    /// successor carry, with the correspondence of the rewrite: what a
    /// host holding a source map of the earlier document rekeys it with.
    pub rewritten_from: Option<(DocumentId, Correspondence)>,
}

/// What a read of a stored workflow document found.
#[derive(Clone, Debug, PartialEq)]
pub enum StoredWorkflow {
    Read(Box<ExecutionDocument>),
    Unretained(Unretained),
}

/// What a read of a stored source map found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum StoredSourceMap {
    /// The map and the source it maps, checked against each other and,
    /// while it is retained, against the document it names.
    Read {
        map: SourceMap,
        source: String,
    },
    Unretained(Unretained),
}

/// Why a document could not be published or read.
#[derive(Debug, thiserror::Error)]
pub enum DocumentStoreError {
    #[error("the document has no canonical encoding: {0}")]
    Encode(#[from] lash_kernel_doc::EncodeError),
    /// The declarations supplied with a document are not the ones it lists.
    #[error("the document's declarations are refused: {0}")]
    Declarations(#[from] lash_kernel_doc::DeclarationRefusal),
    /// A source map is not one of the document and source it is published
    /// with.
    #[error("the source map is refused: {0}")]
    SourceMap(#[from] lash_kernel_doc::SourceMapRefusal),
    /// A retained document does not read at the entry a reference names.
    #[error(transparent)]
    Workflow(#[from] WorkflowDocumentError),
    #[error("stored document `{document}` does not decode: {message}")]
    Corrupt {
        document: DocumentId,
        message: String,
    },
    #[error("stored document `{document}` hashes to `{found}`")]
    Identity {
        document: DocumentId,
        found: DocumentId,
    },
    #[error(transparent)]
    Store(#[from] ArtifactStoreError),
}

impl From<DocumentStoreError> for lash_core::PluginError {
    fn from(error: DocumentStoreError) -> Self {
        match error {
            DocumentStoreError::Store(error) => error.into(),
            error => Self::Runtime(lash_core::RuntimeError::new(
                lash_core::RuntimeErrorCode::RuntimeStoreCorrupt,
                error.to_string(),
            )),
        }
    }
}

/// The store of admitted documents.
#[derive(Clone)]
pub struct KernelDocuments {
    store: Arc<dyn ModuleArtifactStore>,
    /// Pure content residency, shared by clones and a hosting factory's
    /// runtime bindings. Reads still fetch current
    /// bytes, so residency neither retains an artifact nor hides corruption.
    decoded: Arc<Mutex<VecDeque<DecodedDocument>>>,
}

struct DecodedDocument {
    identity: DocumentId,
    bytes: Vec<u8>,
    document: Arc<Document>,
}

/// The hosting factory outlives its per-activation engine bindings. Only
/// pure preparations cross those bindings; each keeps its current store port.
#[derive(Default)]
pub(crate) struct ProcessPreparations {
    decoded: Arc<Mutex<VecDeque<DecodedDocument>>>,
    effects: Arc<Mutex<EffectPreparations>>,
}

impl ProcessPreparations {
    pub(crate) fn bind(
        &self,
        mut engine: super::KernelProcessEngine,
    ) -> super::KernelProcessEngine {
        engine.documents.decoded = Arc::clone(&self.decoded);
        engine.effect_preparations = Arc::clone(&self.effects);
        engine
    }
}

/// Pure effect-set preparation belongs to a hosting factory or standalone
/// engine, shared by its bindings and clones.
/// A registry is immutable once handed to it; retaining its Arc makes both
/// release scope and allocation identity explicit, even across replacement.
#[derive(Default)]
pub(super) struct EffectPreparations {
    entries: VecDeque<PreparedEffects>,
    #[cfg(test)]
    pub(super) preparations: usize,
}

struct PreparedEffects {
    document: DocumentId,
    entry: lash_kernel_doc::Name,
    functions: Arc<lash_kernel_doc::FunctionRegistry>,
    result: Result<lash_core::PerformedEffects, lash_core::ProcessEnvRefusal>,
    bytes: usize,
}

impl EffectPreparations {
    /// Called only after reading and verifying the current stored document
    /// and its entry. This result holds neither host authority nor bindings.
    pub(super) fn effects(
        &mut self,
        identity: DocumentId,
        document: &Document,
        functions: &Arc<lash_kernel_doc::FunctionRegistry>,
        entry: &lash_kernel_doc::Name,
    ) -> Result<lash_core::PerformedEffects, lash_core::ProcessEnvRefusal> {
        if let Some(index) = self.entries.iter().position(|prepared| {
            prepared.document == identity
                && prepared.entry == *entry
                && Arc::ptr_eq(&prepared.functions, functions)
        }) && let Some(hit) = self.entries.remove(index)
        {
            let result = hit.result.clone();
            self.entries.push_back(hit);
            return result;
        }
        #[cfg(test)]
        {
            self.preparations += 1;
        }
        let uncertified =
            |error: String| lash_core::ProcessEnvRefusal::Uncertified { reason: error };
        // Match the existing refusal order: derive the view before the entry's
        // closure. Helper-body graphs are scratch and are dropped here.
        let result = lash_kernel_check::derive_view(document, functions.as_ref())
            .map_err(|error| uncertified(error.to_string()))
            .and_then(|graph| {
                lash_kernel_check::entry_effects(document, &graph, functions.as_ref(), entry)
                    .map_err(|error| uncertified(error.to_string()))
            });
        // Bound sixteen prepared entries by eight MiB of encoded results.
        // Encoding failure or oversized results only bypass residency.
        const BYTES: usize = 8 * 1024 * 1024;
        let bytes = serde_json::to_vec(&result).map_or(BYTES + 1, |bytes| bytes.len());
        if bytes <= BYTES {
            while self.entries.len() >= 16
                || self
                    .entries
                    .iter()
                    .map(|prepared| prepared.bytes)
                    .sum::<usize>()
                    + bytes
                    > BYTES
            {
                self.entries.pop_front();
            }
            self.entries.push_back(PreparedEffects {
                document: identity,
                entry: entry.clone(),
                functions: Arc::clone(functions),
                result: result.clone(),
                bytes,
            });
        }
        result
    }
}

impl std::fmt::Debug for KernelDocuments {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KernelDocuments").finish_non_exhaustive()
    }
}

impl KernelDocuments {
    /// The documents `store` holds.
    pub fn new(store: Arc<dyn ModuleArtifactStore>) -> Self {
        Self {
            store,
            decoded: Arc::new(Mutex::new(VecDeque::new())),
        }
    }

    /// Stores `document` under its identity with `declarations`, the
    /// definition of every library function it lists, each under its own
    /// identity, and adds `claim`'s edge to each: the one writer of kernel
    /// documents. The declarations are checked against the document first;
    /// a declaration already stored is only acquired.
    ///
    /// # Errors
    ///
    /// [`DocumentStoreError`]; a fenced referrer is the store's
    /// `ReferrerEnded`.
    pub async fn publish(
        &self,
        claim: &ReferrerClaim,
        document: &Document,
        declarations: &FunctionDeclarations,
    ) -> Result<DocumentId, DocumentStoreError> {
        lash_kernel_doc::check_declarations(document, declarations)?;
        let identity = document.identity()?;
        for (function, definition) in declarations {
            self.hold(claim, &function_artifact(function), || {
                definition.to_json().map(String::into_bytes)
            })
            .await?;
        }
        self.store
            .publish_module_artifact(claim, &identity.to_string(), document.to_json()?.as_bytes())
            .await?;
        Ok(identity)
    }

    /// Adds `claim`'s edge to the artifact `name`, storing `bytes` first
    /// when nothing holds it.
    async fn hold(
        &self,
        claim: &ReferrerClaim,
        name: &ArtifactName,
        bytes: impl FnOnce() -> Result<Vec<u8>, lash_kernel_doc::EncodeError>,
    ) -> Result<(), DocumentStoreError> {
        match self
            .store
            .acquire_module_artifact(claim, &name.artifact_ref)
            .await
        {
            Ok(()) => Ok(()),
            Err(ArtifactStoreError::ArtifactMissing { .. }) => Ok(self
                .store
                .publish_module_artifact(claim, &name.artifact_ref, &bytes()?)
                .await?),
            Err(error) => Err(error.into()),
        }
    }

    /// The declarations `document` was stored with, each read back under
    /// its identity and checked against it.
    ///
    /// # Errors
    ///
    /// The store's failure to answer.
    pub async fn declarations(
        &self,
        document: &Document,
    ) -> Result<Result<FunctionDeclarations, Unretained>, DocumentStoreError> {
        let mut held = FunctionDeclarations::new();
        for function in document.manifest.functions.keys() {
            let artifact = function_artifact(function);
            let Some(bytes) = self
                .store
                .get_module_artifact(&artifact.artifact_ref)
                .await?
            else {
                return Ok(Err(Unretained::Missing { artifact }));
            };
            let corrupt = |message: String| Unretained::Corrupt {
                artifact: artifact.clone(),
                message,
            };
            let definition = match std::str::from_utf8(&bytes)
                .map_err(|error| error.to_string())
                .and_then(|text| {
                    FunctionDefinition::from_json(text).map_err(|error| error.to_string())
                }) {
                Ok(definition) => definition,
                Err(message) => return Ok(Err(corrupt(message))),
            };
            held.insert(*function, definition);
        }
        Ok(match lash_kernel_doc::check_declarations(document, &held) {
            Ok(()) => Ok(held),
            Err(refusal) => Err(Unretained::Corrupt {
                artifact: match &refusal {
                    lash_kernel_doc::DeclarationRefusal::Identity { function, .. }
                    | lash_kernel_doc::DeclarationRefusal::Name { function, .. }
                    | lash_kernel_doc::DeclarationRefusal::Dependency { function, .. }
                    | lash_kernel_doc::DeclarationRefusal::Encode { function, .. } => {
                        function_artifact(function)
                    }
                    _ => document_artifact(&document.identity()?),
                },
                message: refusal.to_string(),
            }),
        })
    }

    /// The document `reference` names, entered where it says, with the
    /// declarations it was stored with and the rewrite it was made by, if
    /// any. No registry is read: a build that holds none of the document's
    /// helpers reads it as the build that wrote it does.
    ///
    /// # Errors
    ///
    /// The store's failure to answer, or a retained document that does not
    /// read at the entry `reference` names.
    pub async fn workflow(
        &self,
        reference: &WorkflowDocumentRef,
    ) -> Result<StoredWorkflow, DocumentStoreError> {
        let artifact = document_artifact(&reference.document);
        let document = match self.get(&reference.document).await {
            Ok(Some(document)) => document,
            Ok(None) => return Ok(StoredWorkflow::Unretained(Unretained::Missing { artifact })),
            Err(
                error @ (DocumentStoreError::Corrupt { .. } | DocumentStoreError::Identity { .. }),
            ) => {
                return Ok(StoredWorkflow::Unretained(Unretained::Corrupt {
                    artifact,
                    message: error.to_string(),
                }));
            }
            Err(error) => return Err(error),
        };
        let declarations = match self.declarations(&document).await? {
            Ok(declarations) => declarations,
            Err(unretained) => return Ok(StoredWorkflow::Unretained(unretained)),
        };
        let rewritten_from = match self.rewrite(&reference.document).await? {
            Ok(rewrite) => rewrite,
            Err(unretained) => return Ok(StoredWorkflow::Unretained(unretained)),
        };
        let document = WorkflowDocument::entered(document, reference.entry.clone(), declarations)?;
        Ok(StoredWorkflow::Read(Box::new(ExecutionDocument {
            document,
            rewritten_from,
        })))
    }

    /// Records that `correspondence.result` was rewritten from
    /// `correspondence.base`, under `claim`. A document is rewritten from
    /// one base by any one migration; should another base ever rewrite to
    /// the same document, the first record stands, and a map of the other
    /// base refuses to rekey through it (`SourceMap::rekey`).
    ///
    /// # Errors
    ///
    /// [`DocumentStoreError`].
    pub(crate) async fn record_rewrite(
        &self,
        claim: &ReferrerClaim,
        correspondence: &Correspondence,
    ) -> Result<(), DocumentStoreError> {
        match self
            .hold(claim, &rewrite_artifact(&correspondence.result), || {
                correspondence.canonical_bytes()
            })
            .await
        {
            Err(DocumentStoreError::Store(ArtifactStoreError::Immutable { .. })) => Ok(()),
            held => held,
        }
    }

    async fn rewrite(
        &self,
        document: &DocumentId,
    ) -> Result<Result<Option<(DocumentId, Correspondence)>, Unretained>, DocumentStoreError> {
        let artifact = rewrite_artifact(document);
        let Some(bytes) = self
            .store
            .get_module_artifact(&artifact.artifact_ref)
            .await?
        else {
            return Ok(Ok(None));
        };
        Ok(match serde_json::from_slice::<Correspondence>(&bytes) {
            Ok(correspondence) if correspondence.result == *document => {
                Ok(Some((correspondence.base, correspondence)))
            }
            Ok(_) => Err(Unretained::Corrupt {
                artifact,
                message: "the rewrite record names another document".to_owned(),
            }),
            Err(error) => Err(Unretained::Corrupt {
                artifact,
                message: error.to_string(),
            }),
        })
    }

    /// Stores `map` and the `source` it maps under `claim`, once both are
    /// checked against `document`, and answers the map's identity.
    ///
    /// # Errors
    ///
    /// [`DocumentStoreError::SourceMap`] for a map that is not one of
    /// `document` and `source`; nothing is stored for it.
    pub async fn publish_source_map(
        &self,
        claim: &ReferrerClaim,
        document: &Document,
        map: &SourceMap,
        source: &str,
    ) -> Result<SourceMapId, DocumentStoreError> {
        map.validate(document, source)?;
        let identity = map.identity()?;
        self.hold(claim, &source_artifact(&map.source), || {
            Ok(source.as_bytes().to_vec())
        })
        .await?;
        self.hold(claim, &source_map_artifact(&identity), || {
            map.canonical_bytes()
        })
        .await?;
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
    ) -> Result<StoredSourceMap, DocumentStoreError> {
        let artifact = source_map_artifact(identity);
        let Some(bytes) = self
            .store
            .get_module_artifact(&artifact.artifact_ref)
            .await?
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
        if map.identity()? != *identity {
            return corrupt(artifact, "the source map is another map".to_owned());
        }
        let artifact = source_artifact(&map.source);
        let Some(bytes) = self
            .store
            .get_module_artifact(&artifact.artifact_ref)
            .await?
        else {
            return Ok(StoredSourceMap::Unretained(Unretained::Missing {
                artifact,
            }));
        };
        let Ok(source) = String::from_utf8(bytes) else {
            return corrupt(artifact, "the source is not UTF-8".to_owned());
        };
        let checked = match self.get(&map.document).await? {
            Some(document) => map.validate(&document, &source),
            None => map.validate_source(&source),
        };
        match checked {
            Ok(()) => Ok(StoredSourceMap::Read { map, source }),
            Err(refusal) => corrupt(artifact, refusal.to_string()),
        }
    }

    /// Adds `claim`'s edge to a stored document.
    ///
    /// # Errors
    ///
    /// The store's refusal; absent bytes are `ArtifactMissing`.
    pub async fn acquire(
        &self,
        claim: &ReferrerClaim,
        document: &DocumentId,
    ) -> Result<(), ArtifactStoreError> {
        self.store
            .acquire_module_artifact(claim, &document.to_string())
            .await
    }

    /// The document stored under `identity`, if any referrer holds it. The
    /// bytes are checked against the identity they are read under.
    ///
    /// # Errors
    ///
    /// [`DocumentStoreError`].
    pub async fn get(&self, identity: &DocumentId) -> Result<Option<Document>, DocumentStoreError> {
        self.get_shared(identity)
            .await
            .map(|document| document.map(|document| (*document).clone()))
    }

    /// Reuses the immutable document across process activations. The key
    /// includes the stored bytes as well as their checked content identity.
    pub(crate) async fn get_shared(
        &self,
        identity: &DocumentId,
    ) -> Result<Option<Arc<Document>>, DocumentStoreError> {
        let Some(bytes) = self
            .store
            .get_module_artifact(&identity.to_string())
            .await?
        else {
            return Ok(None);
        };
        let mut decoded = self
            .decoded
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(index) = decoded
            .iter()
            .position(|entry| entry.identity == *identity && entry.bytes == bytes)
            && let Some(hit) = decoded.remove(index)
        {
            let document = Arc::clone(&hit.document);
            decoded.push_back(hit);
            return Ok(Some(document));
        }
        let corrupt = |message: String| DocumentStoreError::Corrupt {
            document: *identity,
            message,
        };
        let text = std::str::from_utf8(&bytes).map_err(|error| corrupt(error.to_string()))?;
        let document = Document::from_json(text).map_err(|error| corrupt(error.to_string()))?;
        let found = document.identity()?;
        if found != *identity {
            return Err(DocumentStoreError::Identity {
                document: *identity,
                found,
            });
        }
        let document = Arc::new(document);
        // Residency is an optimization, never an admission limit. Oversized
        // input runs uncached. The bound covers sixteen decoded documents
        // whose encoded content totals at most eight MiB.
        const BYTES: usize = 8 * 1024 * 1024;
        if bytes.len() <= BYTES {
            while decoded.len() >= 16
                || decoded.iter().map(|entry| entry.bytes.len()).sum::<usize>() + bytes.len()
                    > BYTES
            {
                decoded.pop_front();
            }
            decoded.push_back(DecodedDocument {
                identity: *identity,
                bytes,
                document: Arc::clone(&document),
            });
        }
        Ok(Some(document))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stored(Mutex<Option<Vec<u8>>>);

    #[async_trait::async_trait]
    impl ModuleArtifactStore for Stored {
        async fn publish_module_artifact(
            &self,
            _: &ReferrerClaim,
            _: &str,
            bytes: &[u8],
        ) -> Result<(), ArtifactStoreError> {
            *self.0.lock().expect("store") = Some(bytes.to_vec());
            Ok(())
        }
        async fn acquire_module_artifact(
            &self,
            _: &ReferrerClaim,
            _: &str,
        ) -> Result<(), ArtifactStoreError> {
            Ok(())
        }
        async fn end_module_referrer(
            &self,
            _: &lash_core::ResolvedArtifactCleanup,
        ) -> Result<(), ArtifactStoreError> {
            *self.0.lock().expect("store") = None;
            Ok(())
        }
        async fn get_module_artifact(
            &self,
            _: &str,
        ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
            Ok(self.0.lock().expect("store").clone())
        }
    }

    /// FIG-6148, K-DOC-003: repeated activations share decoding, but the
    /// cache cannot keep a reclaimed document runnable or hide changed bytes.
    #[tokio::test]
    async fn shared_decoding_preserves_missing_and_corrupt_document_refusals() {
        let document =
            lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { return 7 }")
                .expect("document");
        let identity = document.identity().expect("identity");
        let store = Arc::new(Stored(Mutex::new(Some(
            document.to_json().expect("json").into_bytes(),
        ))));
        let documents = KernelDocuments::new(store.clone());
        let first = documents
            .get_shared(&identity)
            .await
            .expect("read")
            .expect("retained");
        let repeated = documents
            .clone()
            .get_shared(&identity)
            .await
            .expect("read")
            .expect("retained");
        assert!(Arc::ptr_eq(&first, &repeated));
        *store.0.lock().expect("store") = None;
        assert!(
            documents
                .get_shared(&identity)
                .await
                .expect("read")
                .is_none()
        );
        *store.0.lock().expect("store") = Some(b"broken".to_vec());
        assert!(matches!(
            documents.get_shared(&identity).await,
            Err(DocumentStoreError::Corrupt { .. })
        ));
        let other = lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { return 8 }")
            .expect("document");
        *store.0.lock().expect("store") = Some(other.to_json().expect("json").into_bytes());
        assert!(matches!(
            documents.get_shared(&identity).await,
            Err(DocumentStoreError::Identity { .. })
        ));
    }
    /// FIG-6148: engine activation effect requirements are a structural
    /// preparation of document content under one helper registry, not work
    /// repeated when the same process resumes.
    #[tokio::test]
    async fn activation_effects_are_prepared_once_per_document_and_registry() {
        use lash_core::ProcessEngine as _;
        let mut registry = lash_kernel_doc::FunctionRegistry::new();
        let helper = registry
            .register(
                lash_kernel_doc::parse_definition(
                    "function helper() -> Any\nkernel 1\ncharge 1\nbody { return 7 }",
                )
                .expect("helper"),
                None,
            )
            .expect("registered");
        let document = lash_kernel_doc::parse_document(&format!(
            "kernel 1\nnumbers float\nuse helper = @{helper}\neffect ping(x: Any) -> Any\nentry work() -> Any\nfn work() {{ let n = invoke helper() let x = perform ping(n) as Any return x }}\nmain {{ }}",
        )).expect("document");
        let identity = document.identity().expect("identity");
        let store = Arc::new(Stored(Mutex::new(Some(
            document.to_json().expect("json").into_bytes(),
        ))));
        let config =
            lash_vm_client::PoolConfig::codemode(lash_vm_client::WorkerEntry::helper("unused"));
        let factory = crate::process_plugin::KernelProcessPluginFactory::new(
            lash_vm_client::service::Service::new(config.clone()),
            lash_vm_releases::TYPESCRIPT,
            config.run_bounds,
        )
        .with_functions(Arc::new(registry));
        let payload =
            serde_json::json!({ "document": identity, "functions": [helper], "entry": "work" });
        let mut bindings = Vec::new();
        let mut counts = std::collections::BTreeMap::new();
        let mut expected = None;
        for _ in 0..3 {
            let bound = factory.engine(store.clone()).expect("runtime binding");
            let effects = bound.performed_effects(&payload).await.expect("effects");
            assert_eq!(effects.len(), 1);
            if let Some(expected) = &expected {
                assert_eq!(&effects, expected);
            }
            expected = Some(effects);
            counts.insert(
                Arc::as_ptr(&bound.effect_preparations) as usize,
                bound
                    .effect_preparations
                    .lock()
                    .expect("cache")
                    .preparations,
            );
            bindings.push(bound);
        }
        assert_eq!(
            counts.values().sum::<usize>(),
            1,
            "one preparation across runtime bindings"
        );
        let decoded = bindings[0]
            .documents
            .get_shared(&identity)
            .await
            .expect("document")
            .expect("retained");
        let rebound = bindings[1]
            .documents
            .get_shared(&identity)
            .await
            .expect("document")
            .expect("retained");
        assert!(
            Arc::ptr_eq(&decoded, &rebound),
            "decoding survives runtime rebinding too"
        );
        let absent = factory
            .engine(Arc::new(Stored(Mutex::new(None))))
            .expect("another backend");
        assert!(
            absent.performed_effects(&payload).await.is_err(),
            "content residency confers no store authority"
        );
        let engine = &bindings[0];
        let first = expected.expect("effects");
        let mut other = engine.clone();
        other.functions = Arc::new(lash_kernel_doc::FunctionRegistry::new());
        assert!(other.performed_effects(&payload).await.is_err());
        assert_eq!(
            engine
                .effect_preparations
                .lock()
                .expect("cache")
                .preparations,
            2
        );
        assert_eq!(
            engine
                .performed_effects(&payload)
                .await
                .expect("original registry"),
            first
        );
        *store.0.lock().expect("store") = None;
        assert!(engine.performed_effects(&payload).await.is_err());
    }
}
