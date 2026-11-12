//! What typed authoring knew of a function's source that its executable
//! document does not hold: evidence beside the document, outside its identity.
//!
//! Evidence never proves a construction or admits a program. A printer may
//! use it to spell source, and that source must still lower back to the same
//! document; evidence that is missing is not guessed.
use std::collections::BTreeMap;

use lash_kernel_value::Name;
use lash_kernel_value::names::{DocumentId, FunctionId};
use lash_kernel_value::types::Type;
use lash_kernel_value::{EvidenceSetId, SourceEvidenceId};
use {
    lash_kernel_value::tokens::TokenCodec, lash_kernel_value::tokens::TokenError,
    lash_kernel_value::tokens::TokenReader, lash_kernel_value::tokens::TokenWriter,
};

/// A local binding's declared type (`let cursor: string | null`), keyed by
/// the binding's kernel name. A front end gives no two bindings of one
/// lexical scope chain a name in common, and records no type for a name two
/// bindings of a function and the functions it contains declare differently.
#[derive(
    Clone, Debug, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(deny_unknown_fields)]
pub struct DeclaredLocal {
    pub name: Name,
    pub ty: Type,
}

/// A copied function's source evidence: the implicit conversions its source
/// admitted and the declared types of its locals.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct SourceEvidence {
    pub implicit_text_conversions: Vec<crate::ImplicitTextConversion>,
    pub declared_locals: Vec<DeclaredLocal>,
}

impl SourceEvidence {
    /// The type the binding `name` was declared with, if evidence names one.
    #[must_use]
    pub fn declared(&self, name: &Name) -> Option<&Type> {
        self.declared_locals
            .iter()
            .find(|local| local.name == *name)
            .map(|local| &local.ty)
    }
}

impl TokenCodec for DeclaredLocal {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.name.encode(out);
        self.ty.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            name: Name::decode(input)?,
            ty: Type::decode(input)?,
        })
    }
}

impl TokenCodec for SourceEvidence {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(2);
        self.implicit_text_conversions.encode(out);
        self.declared_locals.encode(out);
    }
    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(2)?;
        Ok(Self {
            implicit_text_conversions: Vec::decode(input)?,
            declared_locals: Vec::decode(input)?,
        })
    }
}

/// Why retained evidence was refused. Evidence is never repaired, dropped
/// or re-identified: a publication or start that carries bad evidence is
/// refused whole.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum EvidenceRefusal {
    #[error("source evidence is not the version this build reads ({found})")]
    Version { found: u64 },
    #[error("source evidence does not decode: {0}")]
    Undecodable(String),
    #[error("source evidence is {actual} bytes, over the {limit}-byte limit")]
    TooLarge { actual: usize, limit: usize },
    #[error("source evidence is not the content its reference names")]
    Digest,
    #[error("source evidence names document {named}, not {actual}")]
    Document {
        named: DocumentId,
        actual: DocumentId,
    },
    #[error("source evidence names {function}, which is no function of its document")]
    Function { function: FunctionId },
    #[error("source evidence declares `{name}` twice in {function}")]
    Duplicate { function: FunctionId, name: Name },
    #[error("source evidence names no evidence for {document}")]
    Missing { document: DocumentId },
}

/// One document's source evidence as a publication retains it: stored as
/// its own artifact beside the document, keyed by its content, and never
/// part of the document's bytes or any executable identity.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EvidenceRecord {
    pub document: DocumentId,
    pub entries: BTreeMap<FunctionId, SourceEvidence>,
}

/// One publication's source evidence: the record each of its documents
/// carries. Two publications of one executable document with different
/// annotations hold different sets, so a run reads its own publication's.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct EvidenceSet {
    pub records: BTreeMap<DocumentId, SourceEvidenceId>,
}

/// The encoding version of [`EvidenceRecord`] and [`EvidenceSet`] bytes. A
/// build reads only its own version; stores are reset at a change.
/// version_surface = "coexist"
/// format_outside_manifest = "kernel artifact-store body, tagged with its version like document bytes"
/// version_guard(
///     shapes(path = "crates/lash-kernel-doc/src/source_evidence.rs", cover(EvidenceRecord, EvidenceSet, SourceEvidence)),
///     items(path = "crates/lash-kernel-doc/src/source_evidence.rs", read_versioned),
/// )
pub const EVIDENCE_VERSION: u64 = 1;

/// The most bytes one evidence record or set may encode to.
pub const EVIDENCE_MAX_BYTES: usize = 4 << 20;

fn versioned(body: impl FnOnce(&mut TokenWriter)) -> Vec<u8> {
    let mut out = TokenWriter::new();
    out.seq(2);
    out.unsigned(EVIDENCE_VERSION);
    body(&mut out);
    out.into_bytes()
}

fn read_versioned<T>(
    bytes: &[u8],
    body: impl FnOnce(&mut TokenReader<'_>) -> Result<T, TokenError>,
) -> Result<T, EvidenceRefusal> {
    if bytes.len() > EVIDENCE_MAX_BYTES {
        return Err(EvidenceRefusal::TooLarge {
            actual: bytes.len(),
            limit: EVIDENCE_MAX_BYTES,
        });
    }
    let undecodable = |error: TokenError| EvidenceRefusal::Undecodable(error.to_string());
    let mut input = TokenReader::new(bytes);
    input.expect_struct(2).map_err(undecodable)?;
    let found = input.unsigned().map_err(undecodable)?;
    if found != EVIDENCE_VERSION {
        return Err(EvidenceRefusal::Version { found });
    }
    let value = body(&mut input).map_err(undecodable)?;
    input.finish().map_err(undecodable)?;
    Ok(value)
}

impl EvidenceRecord {
    /// The record's canonical bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        versioned(|out| {
            out.seq(2);
            self.document.encode(out);
            self.entries.encode(out);
        })
    }

    /// The identity of `bytes` as a record.
    #[must_use]
    pub fn id_of(bytes: &[u8]) -> SourceEvidenceId {
        SourceEvidenceId::from_bytes(blake3::derive_key("lash.source-evidence", bytes))
    }

    /// The record's identity.
    #[must_use]
    pub fn id(&self) -> SourceEvidenceId {
        Self::id_of(&self.encode())
    }

    /// Read the record `id` names from `bytes`, refusing bytes that are not
    /// its content or are no canonical record.
    ///
    /// # Errors
    /// As [`EvidenceRefusal`].
    pub fn decode(bytes: &[u8], id: SourceEvidenceId) -> Result<Self, EvidenceRefusal> {
        let record = read_versioned(bytes, |input| {
            input.expect_struct(2)?;
            Ok(Self {
                document: DocumentId::decode(input)?,
                entries: BTreeMap::decode(input)?,
            })
        })?;
        if Self::id_of(bytes) != id || record.encode() != bytes {
            return Err(EvidenceRefusal::Digest);
        }
        Ok(record)
    }

    /// Check the record against the document it names: each entry is a
    /// function of that document, declaring each local once.
    ///
    /// # Errors
    /// As [`EvidenceRefusal`].
    pub fn validate(
        &self,
        id: DocumentId,
        document: &crate::Document,
    ) -> Result<(), EvidenceRefusal> {
        if self.document != id {
            return Err(EvidenceRefusal::Document {
                named: self.document,
                actual: id,
            });
        }
        for (function, evidence) in &self.entries {
            if !document.functions.contains_key(function) {
                return Err(EvidenceRefusal::Function {
                    function: *function,
                });
            }
            let mut names = std::collections::BTreeSet::new();
            for local in &evidence.declared_locals {
                if !names.insert(&local.name) {
                    return Err(EvidenceRefusal::Duplicate {
                        function: *function,
                        name: local.name.clone(),
                    });
                }
            }
        }
        let size = self.encode().len();
        if size > EVIDENCE_MAX_BYTES {
            return Err(EvidenceRefusal::TooLarge {
                actual: size,
                limit: EVIDENCE_MAX_BYTES,
            });
        }
        Ok(())
    }

    /// The module artifact reference the record is stored under.
    #[must_use]
    pub fn artifact_ref(id: SourceEvidenceId) -> String {
        format!("source-evidence:{id}")
    }
}

impl EvidenceSet {
    /// The set's canonical bytes.
    #[must_use]
    pub fn encode(&self) -> Vec<u8> {
        versioned(|out| self.records.encode(out))
    }

    /// The identity of `bytes` as a set.
    #[must_use]
    pub fn id_of(bytes: &[u8]) -> EvidenceSetId {
        EvidenceSetId::from_bytes(blake3::derive_key("lash.source-evidence-set", bytes))
    }

    /// The set's identity.
    #[must_use]
    pub fn id(&self) -> EvidenceSetId {
        Self::id_of(&self.encode())
    }

    /// Read the set `id` names from `bytes`.
    ///
    /// # Errors
    /// As [`EvidenceRefusal`].
    pub fn decode(bytes: &[u8], id: EvidenceSetId) -> Result<Self, EvidenceRefusal> {
        let set = read_versioned(bytes, |input| {
            Ok(Self {
                records: BTreeMap::decode(input)?,
            })
        })?;
        if Self::id_of(bytes) != id || set.encode() != bytes {
            return Err(EvidenceRefusal::Digest);
        }
        Ok(set)
    }

    /// The module artifact reference the set is stored under: the
    /// publication's evidence hold row, which holds each record it names.
    #[must_use]
    pub fn artifact_ref(id: EvidenceSetId) -> String {
        format!("source-evidence-set:{id}")
    }
}
