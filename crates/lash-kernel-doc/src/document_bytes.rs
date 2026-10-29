//! Opaque host retention and execution form. This is not a durable format:
//! keep the grammar tag with the bytes, and keep typed JSON or source when
//! documents must outlive a grammar change. Lash alone reads the token body.

use std::sync::Arc;

use crate::Document;
use crate::IdentifiedDocument;
use lash_kernel_value::data_limits::KernelDataLimits;
use lash_kernel_value::names::DocumentId;
use lash_kernel_value::tokens::TokenError;

/// The one host token grammar this build accepts. No migration or fallback
/// decoder is provided for opaque host blobs.
/// version_surface = "coexist"
/// version_guard(
///     shapes(path = "crates/lash-kernel-doc/src/document.rs", cover(Document)),
///     items(path = "crates/lash-kernel-doc/src/document_bytes.rs", decode),
/// )
pub const DOCUMENT_GRAMMAR_VERSION: u32 = 5;

/// An opaque document, its lash-issued identity and its grammar tag.
///
/// Hosts copy these three parts without interpreting the bytes. Receipt is
/// untrusted until [`Self::decode`] (also used by publication) validates it.
/// Reading and editing are explicit through [`Self::to_document`]; JSON is
/// available on that typed document, and never supplies identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DocumentBytes {
    source_language: Option<crate::SourceLanguage>,
    source_evidence:
        std::collections::BTreeMap<crate::FunctionId, Vec<crate::ImplicitTextConversion>>,
    grammar_version: u32,
    id: DocumentId,
    body: Arc<[u8]>,
}

impl DocumentBytes {
    /// Reassemble a host's stored blob. This does not validate or admit it:
    /// publication validates all three parts before retaining anything.
    #[must_use]
    pub fn from_parts(grammar_version: u32, id: DocumentId, body: impl Into<Arc<[u8]>>) -> Self {
        Self {
            source_language: None,
            source_evidence: Default::default(),
            grammar_version,
            id,
            body: body.into(),
        }
    }

    /// Retain the authored language for diagnostics. This annotation changes
    /// neither executable bytes nor identity, and is not part of `from_parts`.
    #[must_use]
    pub fn with_source_language(mut self, language: crate::SourceLanguage) -> Self {
        self.source_language = Some(language);
        self
    }

    /// The language retained for diagnostics by source import, if supplied.
    #[must_use]
    pub fn source_language(&self) -> Option<crate::SourceLanguage> {
        self.source_language
    }

    /// Source facts retained by typed authoring, outside executable identity.
    pub fn source_evidence(&self) -> impl Iterator<Item = &crate::ImplicitTextConversion> {
        self.source_evidence.values().flatten()
    }

    #[must_use]
    pub fn grammar_version(&self) -> u32 {
        self.grammar_version
    }

    /// Lash's identity, unverified until receipt validation succeeds.
    #[must_use]
    pub fn id(&self) -> DocumentId {
        self.id
    }

    /// Store or send verbatim; do not interpret or modify these bytes.
    #[must_use]
    pub fn as_bytes(&self) -> &[u8] {
        &self.body
    }

    /// Lash's receipt validation: grammar, canonical token decoding under
    /// the deployment's recursion limits, then agreement with the supplied id.
    /// This does not admit the program against an execution environment.
    ///
    /// # Errors
    /// An unsupported grammar, malformed or over-limit tokens, or wrong id.
    pub fn decode(
        &self,
        limits: KernelDataLimits,
    ) -> Result<IdentifiedDocument, DocumentBytesRefusal> {
        if self.grammar_version != DOCUMENT_GRAMMAR_VERSION {
            return Err(DocumentBytesRefusal::GrammarVersion {
                found: self.grammar_version,
                supported: DOCUMENT_GRAMMAR_VERSION,
            });
        }
        let document = IdentifiedDocument::decode(&self.body, limits)?;
        if document.id() != self.id {
            return Err(DocumentBytesRefusal::Identity {
                supplied: self.id,
                actual: document.id(),
            });
        }
        Ok(document)
    }

    /// Explicitly read a host blob as a typed document, under standard
    /// limits. Editors turn the resulting document back into bytes with
    /// `DocumentBytes::from(document)` after their edits.
    ///
    /// # Errors
    /// As [`Self::decode`].
    pub fn to_document(&self) -> Result<Document, DocumentBytesRefusal> {
        let mut document = self.decode(KernelDataLimits::standard())?.into_document();
        document.source_evidence = self.source_evidence.clone();
        Ok(document)
    }
}

impl From<&IdentifiedDocument> for DocumentBytes {
    fn from(document: &IdentifiedDocument) -> Self {
        let mut bytes = Self::from_parts(
            DOCUMENT_GRAMMAR_VERSION,
            document.id(),
            Arc::clone(document.shared_body()),
        );
        bytes.source_evidence = document.source_evidence.clone();
        bytes
    }
}

impl From<IdentifiedDocument> for DocumentBytes {
    fn from(document: IdentifiedDocument) -> Self {
        Self::from(&document)
    }
}

impl From<Document> for DocumentBytes {
    fn from(document: Document) -> Self {
        Self::from(IdentifiedDocument::new(document))
    }
}

/// A host's blob was refused before semantic admission or publication.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[non_exhaustive]
pub enum DocumentBytesRefusal {
    #[error("document token grammar {found} is unsupported; this build accepts {supported}")]
    GrammarVersion { found: u32, supported: u32 },
    #[error("document identity {supplied} does not match the bytes' identity {actual}")]
    Identity {
        supplied: DocumentId,
        actual: DocumentId,
    },
    #[error("document tokens refused: {0}")]
    Tokens(#[from] TokenError),
}
