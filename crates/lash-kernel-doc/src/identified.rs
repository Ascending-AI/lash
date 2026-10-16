//! A document as lash holds it between components: decoded once, with its
//! identity and the canonical token body that identity is the digest of.
//!
//! A document crosses lash's own seams (a worker's IPC, a cell's envelope,
//! the stores) as its logical token body, the form its checkpoint graph node
//! is encoded from (`K-ID-003`). Whoever lowers or decodes a document makes
//! one [`IdentifiedDocument`] of it, and every later reader shares the
//! decoded value, the identity and the bytes: none of them encodes,
//! decodes or identifies it again. JSON is the host's readable form only
//! ([`Document::to_json`]).

use std::sync::Arc;

use crate::graph_tokens::{TokenCodec as _, TokenError, TokenReader, TokenWriter, logical_digest};
use crate::{Document, DocumentId, KernelDataLimits};

/// A document, its identity and its canonical token body, made together
/// once. Clones share all three.
#[derive(Clone, Debug)]
pub struct IdentifiedDocument {
    document: Arc<Document>,
    id: DocumentId,
    body: Arc<[u8]>,
}

impl IdentifiedDocument {
    /// `document` with its body and identity: one token encode.
    #[must_use]
    pub fn new(document: Document) -> Self {
        Self::from_shared(Arc::new(document))
    }

    /// [`Self::new`] for a document already shared.
    #[must_use]
    pub fn from_shared(document: Arc<Document>) -> Self {
        let mut out = TokenWriter::new();
        document.encode(&mut out);
        let body: Arc<[u8]> = out.into_bytes().into();
        Self {
            id: DocumentId::from_bytes(logical_digest(&body)),
            document,
            body,
        }
    }

    /// The document `body`, a logical token body, holds, under `limits`:
    /// one token decode. The reader refuses every non-canonical spelling,
    /// so the bytes read are the canonical body and their digest is the
    /// document's identity: nothing is encoded again.
    ///
    /// # Errors
    ///
    /// Any token the canonical encoding of a document does not hold, or
    /// trailing bytes.
    pub fn decode(body: &[u8], limits: KernelDataLimits) -> Result<Self, TokenError> {
        let mut input = TokenReader::new(body).with_data_limits(limits);
        let document = Document::decode(&mut input)?;
        input.finish()?;
        Ok(Self {
            id: DocumentId::from_bytes(logical_digest(body)),
            document: Arc::new(document),
            body: body.into(),
        })
    }

    /// [`Self::decode`] under the standard limits.
    ///
    /// # Errors
    ///
    /// As [`Self::decode`].
    pub fn decode_standard(body: &[u8]) -> Result<Self, TokenError> {
        Self::decode(body, KernelDataLimits::standard())
    }

    #[must_use]
    pub fn document(&self) -> &Document {
        &self.document
    }

    /// The decoded document, shared.
    #[must_use]
    pub fn shared(&self) -> &Arc<Document> {
        &self.document
    }

    /// The document's identity (`K-ID-001`).
    #[must_use]
    pub fn id(&self) -> DocumentId {
        self.id
    }

    /// The document's canonical token body (`K-ID-003`).
    #[must_use]
    pub fn body(&self) -> &[u8] {
        &self.body
    }

    /// The token body, shared.
    #[must_use]
    pub fn shared_body(&self) -> &Arc<[u8]> {
        &self.body
    }

    /// The document, without cloning it when this is its only holder.
    #[must_use]
    pub fn into_document(self) -> Document {
        Arc::try_unwrap(self.document).unwrap_or_else(|shared| (*shared).clone())
    }
}

impl std::ops::Deref for IdentifiedDocument {
    type Target = Document;

    fn deref(&self) -> &Document {
        &self.document
    }
}

/// Two documents are equal exactly when their identities are (`K-ID-001`).
impl PartialEq for IdentifiedDocument {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Eq for IdentifiedDocument {}

impl From<Document> for IdentifiedDocument {
    fn from(document: Document) -> Self {
        Self::new(document)
    }
}

/// How many times this thread encoded and decoded a whole document, in
/// each form: what a law counts a document's trips by. Counting is a
/// thread-local add, always on.
pub mod document_work {
    use std::cell::Cell;

    /// Whole-document encodes and decodes, by form.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct DocumentWork {
        pub token_encodes: u64,
        pub token_decodes: u64,
        pub json_encodes: u64,
        pub json_decodes: u64,
    }

    std::thread_local! {
        static WORK: Cell<DocumentWork> = const {
            Cell::new(DocumentWork {
                token_encodes: 0,
                token_decodes: 0,
                json_encodes: 0,
                json_decodes: 0,
            })
        };
    }

    pub(crate) fn note(change: impl FnOnce(&mut DocumentWork)) {
        let _ = WORK.try_with(|work| {
            let mut counted = work.get();
            change(&mut counted);
            work.set(counted);
        });
    }

    /// What this thread did since the last take, and resets it.
    #[must_use]
    pub fn take() -> DocumentWork {
        WORK.try_with(|work| work.replace(DocumentWork::default()))
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::NumberPolicy;

    fn document() -> Document {
        crate::parse_document("kernel 1\nnumbers float\nmain { print 1 }").expect("document")
    }

    /// One encode makes the body and the identity; one decode of that body
    /// answers the same identity with nothing encoded.
    #[test]
    fn an_identified_document_is_encoded_or_decoded_once_and_keeps_its_identity() {
        let document = document();
        let _ = document_work::take();
        let made = IdentifiedDocument::new(document);
        assert_eq!(
            document_work::take(),
            document_work::DocumentWork {
                token_encodes: 1,
                ..Default::default()
            }
        );
        let read = IdentifiedDocument::decode_standard(made.body()).expect("decodes");
        assert_eq!(
            document_work::take(),
            document_work::DocumentWork {
                token_decodes: 1,
                ..Default::default()
            }
        );
        assert_eq!(read.id(), made.id());
        assert_eq!(made.id(), made.document().identity());
        assert_eq!(read.document(), made.document());
        assert_ne!(
            made.id(),
            IdentifiedDocument::new(Document::new(NumberPolicy::BySpelling, Vec::new())).id()
        );
    }
}
