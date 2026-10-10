//! The workflow document: a kernel program and what it requires.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ast::{Block, Function, Site};
use crate::canonical::{EncodeError, digest, json_depth};
use crate::name::{DocumentId, EffectName, FunctionId, FunctionName, Name};
use crate::number::NumberPolicy;
use crate::types::Signature;

/// The kernel version this crate's forms, values, evaluation order,
/// statement rule, task scheduling, site derivation and canonical form are
/// written for (`K-VER-001`): the number of [`crate::KernelVersion::One`],
/// and what a front end and a library definition state. A build may
/// interpret a second version beside it (`K-VER-003`).
pub const KERNEL_VERSION: u32 = 1;

/// The deepest a node may sit below its unit's body, counted in
/// [`crate::Node::children`] steps (`K-DOC-006`). A block and each statement
/// in it are two levels.
pub const MAX_NESTING_DEPTH: usize = 64;

/// The deepest array-and-object nesting a JSON document or definition may
/// have. No tree within [`MAX_NESTING_DEPTH`] encodes deeper.
const MAX_JSON_DEPTH: usize = MAX_NESTING_DEPTH * 5 + 32;

/// A workflow document: `main`, the declared functions, the entries a host
/// may start, the private bindings and the requirements manifest.
///
/// A document holds behaviour only. Labels, layout, the dialect it was
/// written in and its authored source are [`Annotations`], stored apart and
/// keyed to the document's [`DocumentId`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub manifest: Manifest,
    /// The declared functions, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub functions: BTreeMap<Name, Function>,
    /// The declared functions a host may start, each with the typed
    /// signature it is started under.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entries: BTreeMap<Name, Signature>,
    /// The variables of `main` that are the program's own. Every other
    /// variable `main` declares at its top level is a session binding.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub private_bindings: BTreeSet<Name>,
    pub main: Block,
}

/// What a document needs from the environment that runs it. Admission
/// refuses a document whose requirements the environment does not meet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The kernel version the document is written in.
    pub kernel: u32,
    /// Every effect the document performs, with the signature it expects.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effects: BTreeMap<EffectName, Signature>,
    /// Every library function the document reaches, directly or through
    /// another function's body, by identity, with the name its definition
    /// carries.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub functions: BTreeMap<FunctionId, FunctionName>,
    /// How an effect result's bare number decodes.
    pub numbers: NumberPolicy,
}

impl Manifest {
    /// A manifest for [`KERNEL_VERSION`] that requires nothing.
    pub fn new(numbers: NumberPolicy) -> Self {
        Self {
            kernel: KERNEL_VERSION,
            effects: BTreeMap::new(),
            functions: BTreeMap::new(),
            numbers,
        }
    }
}

impl Document {
    /// A document with this `main` and nothing else.
    pub fn new(numbers: NumberPolicy, main: Block) -> Self {
        Self {
            manifest: Manifest::new(numbers),
            functions: BTreeMap::new(),
            entries: BTreeMap::new(),
            private_bindings: BTreeSet::new(),
            main,
        }
    }

    /// The document's behavioural identity (`K-ID-001`).
    pub fn identity(&self) -> Result<DocumentId, EncodeError> {
        digest("lash-kernel-document", self).map(DocumentId::from_bytes)
    }

    pub fn to_json(&self) -> Result<String, EncodeError> {
        to_json(self)
    }

    pub fn from_json(text: &str) -> Result<Self, DecodeError> {
        from_versioned_json(text)
    }
}

/// A JSON text that is not the shape asked for.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum DecodeError {
    #[error("JSON nests {depth} deep; the limit is {limit}")]
    TooDeep { depth: usize, limit: usize },
    #[error("{message} at line {line} column {column}")]
    Invalid {
        message: String,
        line: usize,
        column: usize,
    },
}

pub(crate) fn to_json<T: Serialize>(value: &T) -> Result<String, EncodeError> {
    #[cfg(feature = "synthetic-next")]
    if let Some(tree) = crate::version::synthetic::encode(value)? {
        return serde_json::to_string(&tree).map_err(|error| EncodeError {
            message: error.to_string(),
        });
    }
    serde_json::to_string(value).map_err(|error| EncodeError {
        message: error.to_string(),
    })
}

pub(crate) fn from_json<T: DeserializeOwned>(text: &str) -> Result<T, DecodeError> {
    let depth = json_depth(text);
    if depth > MAX_JSON_DEPTH {
        return Err(DecodeError::TooDeep {
            depth,
            limit: MAX_JSON_DEPTH,
        });
    }
    #[cfg(feature = "synthetic-next")]
    if let Some(value) = crate::version::synthetic::decode(text)? {
        return Ok(value);
    }
    let mut decoder = serde_json::Deserializer::from_str(text);
    decoder.disable_recursion_limit();
    let invalid = |error: serde_json::Error| DecodeError::Invalid {
        message: error.to_string(),
        line: error.line(),
        column: error.column(),
    };
    let value = T::deserialize(&mut decoder).map_err(invalid)?;
    decoder.end().map_err(invalid)?;
    Ok(value)
}

/// Read only the version envelope before decoding any forms (K-VER-003).
/// Unknown fields are skipped by serde, so a payload malformed for this
/// build cannot disguise an unsupported version as a form decoding fault.
pub(crate) fn from_versioned_json<T: DeserializeOwned>(text: &str) -> Result<T, DecodeError> {
    #[derive(Deserialize)]
    struct Version {
        kernel: u32,
    }
    #[derive(Deserialize)]
    struct Header {
        #[serde(default)]
        kernel: Option<u32>,
        #[serde(default)]
        manifest: Option<Version>,
    }
    let header: Header = from_json(text)?;
    let kernel = header
        .manifest
        .map(|manifest| manifest.kernel)
        .or(header.kernel);
    if let Some(found) = kernel.filter(|kernel| crate::KernelVersion::of(*kernel).is_none()) {
        return Err(DecodeError::Invalid {
            message: format!(
                "unsupported kernel version {found}; newest supported is {}",
                crate::KernelVersion::NEWEST
            ),
            line: 0,
            column: 0,
        });
    }
    from_json(text)
}

/// The annotation layer of one document: everything about it that is not
/// behaviour.
///
/// Annotations are keyed to the document's identity and attached to nodes.
/// They never change what runs, and dropping them loses nothing a run needs.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Annotations {
    /// The document these annotate.
    pub document: DocumentId,
    /// The dialect the document was written in.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub dialect: Option<String>,
    /// The source text the document was lowered from.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source: Option<String>,
    /// Per-node annotations, ordered by site, at most one per site.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeAnnotation>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct NodeAnnotation {
    pub site: Site,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<Label>,
    /// Anything else a host or a dialect keeps on the node (layout, a source
    /// span), under a key of its own choosing. The kernel reads none of it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub data: BTreeMap<String, serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Label {
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

impl Annotations {
    /// An empty annotation layer for `document`.
    pub fn new(document: DocumentId) -> Self {
        Self {
            document,
            dialect: None,
            source: None,
            nodes: Vec::new(),
        }
    }

    pub fn to_json(&self) -> Result<String, EncodeError> {
        to_json(self)
    }

    pub fn from_json(text: &str) -> Result<Self, DecodeError> {
        from_json(text)
    }
}
