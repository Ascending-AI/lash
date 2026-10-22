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

use crate::Document;
use lash_vocabulary::data_limits::KernelDataLimits;
use lash_vocabulary::names::DocumentId;
use lash_vocabulary::tokens::TokenCodec as _;
use lash_vocabulary::tokens::TokenError;
use lash_vocabulary::tokens::TokenReader;
use lash_vocabulary::tokens::TokenWriter;
use lash_vocabulary::tokens::logical_digest;

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

/// This thread's document trips and kernel-text definition parses.
/// Each counter is off until this thread calls its corresponding take
/// function. Only aggregate counts are retained, never documents,
/// identities or guest values (ADR 0123).
pub mod document_work {
    use std::cell::Cell;

    std::thread_local! {
        static DEFINITION_PARSES: Cell<Option<u64>> = const { Cell::new(None) };
    }

    pub(crate) fn note_definition_parse() {
        let _ = DEFINITION_PARSES.try_with(|count| {
            if let Some(parses) = count.get() {
                count.set(Some(parses + 1));
            }
        });
    }

    /// Opts this thread into counting kernel-text function parses and
    /// starts a fresh interval. Returns the count since its last take,
    /// or zero on its first take. Counting stays enabled on this thread.
    #[must_use]
    pub fn take_definition_parses() -> u64 {
        DEFINITION_PARSES.with(|count| count.replace(Some(0)).unwrap_or_default())
    }

    /// Whole-document encodes and decodes, by form.
    #[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
    pub struct DocumentWork {
        pub token_encodes: u64,
        pub token_decodes: u64,
        pub json_encodes: u64,
        pub json_decodes: u64,
    }

    std::thread_local! {
        static WORK: Cell<Option<DocumentWork>> = const { Cell::new(None) };
    }

    pub(crate) fn note(change: impl FnOnce(&mut DocumentWork)) {
        let _ = WORK.try_with(|work| {
            let Some(mut counted) = work.get() else {
                return;
            };
            change(&mut counted);
            work.set(Some(counted));
        });
    }

    /// Opts this thread into counting and starts a fresh interval. Returns
    /// the counts since its last take, or zero on its first take. Counting
    /// stays enabled on this thread for subsequent intervals.
    #[must_use]
    pub fn take() -> DocumentWork {
        WORK.try_with(|work| {
            work.replace(Some(DocumentWork::default()))
                .unwrap_or_default()
        })
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

    /// ADR 0123's opt-in rule also covers the definition parser's probe.
    #[test]
    fn definition_parse_work_is_opt_in_on_each_thread() {
        std::thread::spawn(|| {
            let definition =
                "function test.identity(x: Any) -> Any\nkernel 1\ncharge 1\nbody { return x }\n";
            let _ = crate::parse_definition(definition).expect("definition");
            assert_eq!(
                document_work::take_definition_parses(),
                0,
                "definition parse work is disabled until this thread opts in"
            );
            let _ = crate::parse_definition(definition).expect("definition");
            assert_eq!(document_work::take_definition_parses(), 1);
            assert_eq!(document_work::take_definition_parses(), 0);
        })
        .join()
        .expect("definition parse work law");
    }

    /// ADR 0123 admits opt-in diagnostics: an ordinary thread retains no
    /// document work, and taking counts explicitly starts its interval.
    #[test]
    fn document_work_is_opt_in_on_each_thread() {
        std::thread::spawn(|| {
            let document = document();
            let _ = document.canonical_bytes();
            assert_eq!(
                document_work::take(),
                document_work::DocumentWork::default(),
                "document work is disabled until this thread opts in"
            );
            let made = IdentifiedDocument::new(document);
            let _ = IdentifiedDocument::decode_standard(made.body()).expect("decodes");
            assert_eq!(
                document_work::take(),
                document_work::DocumentWork {
                    token_encodes: 1,
                    token_decodes: 1,
                    ..Default::default()
                }
            );
            assert_eq!(
                document_work::take(),
                document_work::DocumentWork::default()
            );
        })
        .join()
        .expect("document work law");
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
