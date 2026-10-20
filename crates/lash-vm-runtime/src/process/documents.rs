//! The admitted documents processes run, in the store set's module port.
//!
//! A document is stored once, as its checkpoint graph node (kind 10), under
//! its identity: the digest of its canonical token body, kernel version and
//! manifest included (`K-DOC-003`). The artifact's row roots that node, so
//! a cell checkpoint naming the same document shares the one copy
//! (FIG-6469). [`KernelDocuments::publish`] is the one writer: beside the
//! document it holds the definition of every library function the document
//! lists, each as its HelperDefinition node (kind 14) under its own
//! identity, so a reader draws the
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

use lash_core::ArtifactName;
use lash_core::ReferrerClaim;
use lash_execution_contracts::module_artifacts::ArtifactStoreError;
use lash_execution_contracts::module_artifacts::ModuleArtifactBody;
use lash_execution_contracts::module_artifacts::ModuleArtifactHold;
use lash_execution_contracts::module_artifacts::ModuleArtifactStore;
use lash_kernel_check::{WorkflowDocument, WorkflowDocumentError};
use lash_kernel_doc::graph_tokens::decode_body;
use lash_kernel_doc::{
    CheckedDeclarations, Correspondence, Document, DocumentId, FunctionDeclarations,
    FunctionDefinition, FunctionId, IdentifiedDocument, SourceMap, SourceMapId,
    WorkflowDocumentRef,
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

impl From<DocumentStoreError> for lash_execution_contracts::plugin_error::PluginError {
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
    /// Each entry is keyed by its token body.
    decoded: Arc<Mutex<VecDeque<IdentifiedDocument>>>,
}

/// The hosting factory outlives its per-activation engine bindings. Only
/// pure preparations cross those bindings; each keeps its current store port.
#[derive(Default)]
pub(crate) struct ProcessPreparations {
    decoded: Arc<Mutex<VecDeque<IdentifiedDocument>>>,
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
    result:
        Result<lash_execution_contracts::process::PerformedEffects, lash_core::ProcessEnvRefusal>,
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
    ) -> Result<lash_execution_contracts::process::PerformedEffects, lash_core::ProcessEnvRefusal>
    {
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

    /// Stores `document` under its identity as its token body, with
    /// `declarations`, the definition of every library function it lists,
    /// each under its own identity as its token body, and adds `claim`'s edge to each, in one store transaction:
    /// the one writer of kernel documents. The declarations are checked
    /// against the document first; a declaration already stored is only
    /// acquired.
    ///
    /// # Errors
    ///
    /// [`DocumentStoreError`]; a fenced referrer is the store's
    /// `ReferrerEnded`.
    pub async fn publish(
        &self,
        claim: &ReferrerClaim,
        document: &IdentifiedDocument,
        declarations: &CheckedDeclarations,
    ) -> Result<DocumentId, DocumentStoreError> {
        self.publish_held(claim, None, document, declarations).await
    }

    /// [`Self::publish`], with the declarations held by `retainer` and the
    /// document by `claim`: a referrer that outlives `claim` and retains
    /// what many documents list, such as the frame whose cells list the
    /// same helpers (FIG-6481). An acquisition `retainer` already holds
    /// writes nothing, so a document whose declarations are all retained
    /// writes only itself. The declarations are held first, so a stored
    /// document's are always held; a refusal of the document leaves them
    /// to `retainer`, which reclaims them at its end.
    ///
    /// # Errors
    ///
    /// [`DocumentStoreError`]; a fenced referrer is the store's
    /// `ReferrerEnded`.
    pub async fn publish_retained(
        &self,
        claim: &ReferrerClaim,
        retainer: &ReferrerClaim,
        document: &IdentifiedDocument,
        declarations: &CheckedDeclarations,
    ) -> Result<DocumentId, DocumentStoreError> {
        self.publish_held(claim, Some(retainer), document, declarations)
            .await
    }

    /// The one writer: `document` under `claim`, its declarations under
    /// `retainer`, or beside it in the same transaction when there is none.
    async fn publish_held(
        &self,
        claim: &ReferrerClaim,
        retainer: Option<&ReferrerClaim>,
        document: &IdentifiedDocument,
        declarations: &CheckedDeclarations,
    ) -> Result<DocumentId, DocumentStoreError> {
        declarations.check(document)?;
        let identity = document.id();
        let functions: Vec<(ArtifactName, Arc<[u8]>)> = declarations
            .images()
            .into_iter()
            .map(|(function, image)| (function_artifact(&function), image))
            .collect();
        let document_ref = identity.to_string();
        let mut artifacts: Vec<ModuleArtifactHold<'_>> = functions
            .iter()
            .map(|(name, image)| ModuleArtifactHold::Acquire {
                module_ref: &name.artifact_ref,
                body: ModuleArtifactBody::HelperDefinition(image),
            })
            .collect();
        let document_hold = ModuleArtifactHold::Publish {
            module_ref: &document_ref,
            body: ModuleArtifactBody::Document(document.body()),
        };
        match retainer {
            None => {
                artifacts.push(document_hold);
                self.store.hold_module_artifacts(claim, &artifacts).await?;
            }
            Some(retainer) => {
                self.store
                    .hold_module_artifacts(retainer, &artifacts)
                    .await?;
                self.store
                    .hold_module_artifacts(claim, &[document_hold])
                    .await?;
            }
        }
        Ok(identity)
    }

    /// Adds `claim`'s edge to the artifact `name`, storing `bytes` first
    /// when nothing holds it.
    async fn hold(
        &self,
        claim: &ReferrerClaim,
        name: &ArtifactName,
        bytes: &[u8],
    ) -> Result<(), DocumentStoreError> {
        Ok(self
            .store
            .hold_module_artifacts(
                claim,
                &[ModuleArtifactHold::Acquire {
                    module_ref: &name.artifact_ref,
                    body: ModuleArtifactBody::Bytes(bytes),
                }],
            )
            .await?)
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
            let definition = match decode_body::<FunctionDefinition>(&bytes) {
                Ok(definition) => definition,
                Err(error) => return Ok(Err(corrupt(error.to_string()))),
            };
            held.insert(*function, definition);
        }
        Ok(match lash_kernel_doc::check_declarations(document, &held) {
            Ok(()) => Ok(held),
            Err(refusal) => Err(Unretained::Corrupt {
                artifact: match &refusal {
                    lash_kernel_doc::DeclarationRefusal::Identity { function, .. }
                    | lash_kernel_doc::DeclarationRefusal::Name { function, .. }
                    | lash_kernel_doc::DeclarationRefusal::Dependency { function, .. } => {
                        function_artifact(function)
                    }
                    _ => document_artifact(&document.identity()),
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
        let bytes =
            serde_json::to_vec(correspondence).map_err(|error| lash_kernel_doc::EncodeError {
                message: error.to_string(),
            })?;
        self.hold(claim, &rewrite_artifact(&correspondence.result), &bytes)
            .await
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
        let identity = map.identity();
        let map_bytes = map.canonical_bytes();
        self.store
            .hold_module_artifacts(
                claim,
                &[
                    ModuleArtifactHold::Acquire {
                        module_ref: &source_artifact(&map.source).artifact_ref,
                        body: ModuleArtifactBody::Bytes(source.as_bytes()),
                    },
                    ModuleArtifactHold::Acquire {
                        module_ref: &source_map_artifact(&identity).artifact_ref,
                        body: ModuleArtifactBody::Bytes(&map_bytes),
                    },
                ],
            )
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
        if map.identity() != *identity {
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
        let checked = match self.get_shared(&map.document).await? {
            Some(document) => map.validate_identified(&document, &source),
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
            .map(|document| document.map(IdentifiedDocument::into_document))
    }

    /// Reuses the immutable document across process activations. The key
    /// includes the stored bytes as well as their checked content identity.
    /// A miss decodes the stored token body once; its digest is checked
    /// against the identity it was read under.
    pub(crate) async fn get_shared(
        &self,
        identity: &DocumentId,
    ) -> Result<Option<IdentifiedDocument>, DocumentStoreError> {
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
            .position(|entry| entry.id() == *identity && entry.body() == bytes.as_slice())
            && let Some(hit) = decoded.remove(index)
        {
            let document = hit.clone();
            decoded.push_back(hit);
            return Ok(Some(document));
        }
        let document = IdentifiedDocument::decode_standard(&bytes).map_err(|error| {
            DocumentStoreError::Corrupt {
                document: *identity,
                message: error.to_string(),
            }
        })?;
        let found = document.id();
        if found != *identity {
            return Err(DocumentStoreError::Identity {
                document: *identity,
                found,
            });
        }
        // Residency is an optimization, never an admission limit. Oversized
        // input runs uncached. The bound covers sixteen decoded documents
        // whose encoded content totals at most eight MiB.
        const BYTES: usize = 8 * 1024 * 1024;
        if bytes.len() <= BYTES {
            while decoded.len() >= 16
                || decoded
                    .iter()
                    .map(|entry| entry.body().len())
                    .sum::<usize>()
                    + bytes.len()
                    > BYTES
            {
                decoded.pop_front();
            }
            decoded.push_back(document.clone());
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
        async fn hold_module_artifacts(
            &self,
            _: &ReferrerClaim,
            artifacts: &[ModuleArtifactHold<'_>],
        ) -> Result<(), ArtifactStoreError> {
            if let Some(last) = artifacts.last() {
                *self.0.lock().expect("store") = Some(last.body().bytes().to_vec());
            }
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

    /// One store write a retention made: each artifact it holds, with the
    /// address of the bytes it was handed.
    type Write = Vec<(String, usize)>;

    /// Every store write a retention makes.
    #[derive(Default)]
    struct Counted {
        stored: Mutex<std::collections::BTreeMap<String, Vec<u8>>>,
        /// Each artifact held as a graph node's body, and the node's kind.
        nodes: Mutex<std::collections::BTreeMap<String, u32>>,
        writes: Mutex<Vec<Write>>,
    }

    #[async_trait::async_trait]
    impl ModuleArtifactStore for Counted {
        async fn publish_module_artifact(
            &self,
            claim: &ReferrerClaim,
            module_ref: &str,
            bytes: &[u8],
        ) -> Result<(), ArtifactStoreError> {
            self.hold_module_artifacts(
                claim,
                &[ModuleArtifactHold::Publish {
                    module_ref,
                    body: ModuleArtifactBody::Bytes(bytes),
                }],
            )
            .await
        }
        async fn acquire_module_artifact(
            &self,
            _: &ReferrerClaim,
            module_ref: &str,
        ) -> Result<(), ArtifactStoreError> {
            self.writes
                .lock()
                .expect("writes")
                .push(vec![(module_ref.to_owned(), 0)]);
            Err(ArtifactStoreError::ArtifactMissing {
                artifact_ref: module_ref.to_owned(),
            })
        }
        async fn hold_module_artifacts(
            &self,
            _: &ReferrerClaim,
            artifacts: &[ModuleArtifactHold<'_>],
        ) -> Result<(), ArtifactStoreError> {
            let mut stored = self.stored.lock().expect("store");
            let mut write = Write::new();
            for artifact in artifacts {
                let bytes = artifact.body().bytes();
                stored
                    .entry(artifact.module_ref().to_owned())
                    .or_insert_with(|| bytes.to_vec());
                if let Some(kind) = artifact.body().node_kind() {
                    self.nodes
                        .lock()
                        .expect("nodes")
                        .insert(artifact.module_ref().to_owned(), kind.0);
                }
                write.push((artifact.module_ref().to_owned(), bytes.as_ptr() as usize));
            }
            self.writes.lock().expect("writes").push(write);
            Ok(())
        }
        async fn end_module_referrer(
            &self,
            _: &lash_core::ResolvedArtifactCleanup,
        ) -> Result<(), ArtifactStoreError> {
            self.stored.lock().expect("store").clear();
            Ok(())
        }
        async fn get_module_artifact(
            &self,
            module_ref: &str,
        ) -> Result<Option<Vec<u8>>, ArtifactStoreError> {
            Ok(self.stored.lock().expect("store").get(module_ref).cloned())
        }
    }

    /// DECLARATIONS-RETAIN-ONCE (FIG-6457): a cell retains its document and
    /// the K helper declarations it lists in one fenced store write, handing
    /// the store the token bodies the registry encoded once, every cell
    /// alike; the declarations still read back with the document.
    #[tokio::test]
    async fn declarations_retain_once_in_one_fenced_batch() {
        let mut registry = lash_kernel_doc::FunctionRegistry::new();
        let mut header = String::from("kernel 1\nnumbers float\n");
        let mut calls = String::new();
        for (name, local) in [
            ("helper.one", "a"),
            ("helper.two", "b"),
            ("helper.three", "c"),
        ] {
            calls.push_str(&format!("let {local} = invoke {name}() "));
            let function = registry
                .register(
                    lash_kernel_doc::parse_definition(&format!(
                        "function {name}() -> Any\nkernel 1\ncharge 1\nbody {{ return 7 }}"
                    ))
                    .expect("helper"),
                    None,
                )
                .expect("registered");
            header.push_str(&format!("use {name} = @{function}\n"));
        }
        let store = Arc::new(Counted::default());
        let documents = KernelDocuments::new(store.clone());
        let mut retained = Vec::new();
        // Each cell's declarations stay alive, so an image encoded again
        // could not reuse the address of the one before.
        let mut checked = Vec::new();
        for cell in [7, 8] {
            let document = IdentifiedDocument::new(
                lash_kernel_doc::parse_document(&format!(
                    "{header}main {{ {calls}return {cell} }}"
                ))
                .expect("document"),
            );
            let claim = ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
                lash_core::HostArtifactPin::mint(),
            ))
            .expect("host claim");
            checked.push(CheckedDeclarations::required(&document, &registry).expect("listed"));
            let identity = documents
                .publish(&claim, &document, checked.last().expect("checked"))
                .await
                .expect("published");
            let mut writes = std::mem::take(&mut *store.writes.lock().expect("writes"));
            assert_eq!(writes.len(), 1, "one fenced batch, not {writes:?}");
            let mut write = writes.remove(0);
            assert_eq!(write.len(), 4);
            assert_eq!(
                write.pop().map(|(artifact, _)| artifact),
                Some(document_artifact(&identity).artifact_ref)
            );
            let read = documents
                .workflow(&WorkflowDocumentRef {
                    document: identity,
                    entry: lash_kernel_doc::WorkflowDocumentEntry::Main,
                })
                .await
                .expect("read");
            assert!(matches!(read, StoredWorkflow::Read(_)), "{read:?}");
            retained.push(write);
        }
        assert_eq!(
            retained[0], retained[1],
            "a later cell holds the same declaration images, encoded once"
        );
    }

    /// FIG-6257, FIG-6469: however many process definitions publish a
    /// document, the store holds it once, as its checkpoint graph node (kind
    /// 10) encoded from its token body, under the digest that keys that
    /// node; each helper it lists is held as its own HelperDefinition node
    /// (kind 14). A read decodes that body once, and nothing is JSON.
    #[tokio::test]
    async fn a_document_is_stored_once_as_its_graph_node_body() {
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
        let document = IdentifiedDocument::new(
            lash_kernel_doc::parse_document(&format!(
                "kernel 1\nnumbers float\nuse helper = @{helper}\nentry one() -> Any\nentry two() -> Any\nfn one() {{ let n = invoke helper() return n }}\nfn two() {{ let n = invoke helper() return n }}\nmain {{ }}",
            ))
            .expect("document"),
        );
        let declarations = CheckedDeclarations::required(&document, &registry).expect("listed");
        let store = Arc::new(Counted::default());
        let documents = KernelDocuments::new(store.clone());
        let claim = ReferrerClaim::unguarded(lash_core::ArtifactReferrer::HostPin(
            lash_core::HostArtifactPin::mint(),
        ))
        .expect("a host pin is unguarded");
        // One definition per entry, each publishing the document it runs.
        for _ in document.entries.keys() {
            assert_eq!(
                documents
                    .publish(&claim, &document, &declarations)
                    .await
                    .expect("published"),
                document.id()
            );
        }
        let held = store.stored.lock().expect("store").clone();
        let stored: Vec<_> = held
            .iter()
            .filter(|(_, bytes)| decode_body::<Document>(bytes).is_ok())
            .collect();
        assert_eq!(stored.len(), 1, "one stored document: {:?}", held.keys());
        let (name, bytes) = stored[0];
        assert_eq!(name, &document.id().to_string());
        assert_eq!(bytes.as_slice(), document.body());
        assert_eq!(
            &lash_kernel_doc::graph_tokens::logical_digest(bytes),
            document.id().as_bytes(),
            "the stored bytes are the body the graph keys by the document's id"
        );
        let definition =
            decode_body::<FunctionDefinition>(&held[&function_artifact(&helper).artifact_ref])
                .expect("the helper is stored as its token body");
        assert_eq!(definition.identity(), helper);
        assert_eq!(
            *store.nodes.lock().expect("nodes"),
            std::collections::BTreeMap::from([
                (document.id().to_string(), 10),
                (function_artifact(&helper).artifact_ref, 14),
            ]),
            "the document and its helper are held as graph nodes, not as copies of their bodies"
        );
        assert!(
            held.values()
                .all(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).is_err()),
            "nothing is stored as JSON"
        );
        let _ = lash_kernel_doc::document_work::take();
        let read = documents
            .get_shared(&document.id())
            .await
            .expect("read")
            .expect("retained");
        assert_eq!(read, document);
        assert_eq!(
            lash_kernel_doc::document_work::take(),
            lash_kernel_doc::document_work::DocumentWork {
                token_decodes: 1,
                ..Default::default()
            }
        );
    }

    /// FIG-6148, K-DOC-003: repeated activations share decoding, but the
    /// cache cannot keep a reclaimed document runnable or hide changed bytes.
    #[tokio::test]
    async fn shared_decoding_preserves_missing_and_corrupt_document_refusals() {
        let document =
            lash_kernel_doc::parse_document("kernel 1\nnumbers float\nmain { return 7 }")
                .expect("document");
        let identity = document.identity();
        let store = Arc::new(Stored(Mutex::new(Some(document.canonical_bytes()))));
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
        assert!(Arc::ptr_eq(first.shared(), repeated.shared()));
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
        *store.0.lock().expect("store") = Some(other.canonical_bytes());
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
        use lash_execution_contracts::process::ProcessEngine as _;
        let package = lash_kernel_package::Package::build(
            "fixture",
            "1",
            &lash_kernel_package::PackageSources {
                kernel: "function fixture.helper() -> Any\nkernel 1\ncharge 1\nbody { return 7 }",
                surface: "",
                labels: "",
            },
            &[],
        )
        .expect("helper package");
        let helper = package.function("fixture.helper").expect("helper identity");
        let held = Arc::new(
            lash_kernel_package::HeldPackages::install([package])
                .expect("one package registration"),
        );
        let document = lash_kernel_doc::parse_document(&format!(
            "kernel 1\nnumbers float\nuse fixture.helper = @{helper}\neffect ping(x: Any) -> Any contract \"1\"\nentry work() -> Any\nfn work() {{ let n = invoke fixture.helper() let x = perform ping(n) as Any return x }}\nmain {{ }}",
        )).expect("document");
        let identity = document.identity();
        let store = Arc::new(Stored(Mutex::new(Some(document.canonical_bytes()))));
        let config =
            lash_vm_client::PoolConfig::codemode(lash_vm_client::WorkerEntry::helper("unused"));
        let factory = crate::process_plugin::KernelProcessPluginFactory::new(
            lash_vm_client::service::Service::new(config.clone()),
            lash_vm_releases::TYPESCRIPT,
            config.run_bounds,
        )
        .with_packages(held);
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
            Arc::ptr_eq(decoded.shared(), rebound.shared()),
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
