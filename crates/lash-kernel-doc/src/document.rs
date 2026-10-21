//! The workflow document: a kernel program and what it requires.

use std::collections::{BTreeMap, BTreeSet};

use schemars::JsonSchema;
use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};

use crate::ast::Block;
use crate::ast::Function;
use crate::ast::Node;
use crate::number::NumberPolicy;
use lash_vocabulary::legacy_names::DocumentId;
use lash_vocabulary::legacy_names::EffectName;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::FunctionName;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::operations::OperationRequirement;
use lash_vocabulary::sites::Site;
use lash_vocabulary::tokens::encode_body;
use lash_vocabulary::tokens::logical_digest;
use lash_vocabulary::types::Signature;

/// The kernel version this crate's forms, values, evaluation order,
/// nested-action positions, task scheduling, site derivation and canonical form are
/// written for (`K-VER-001`): the number of [`crate::KernelVersion::One`],
/// and what a front end and a library definition state. A build may
/// interpret a second version beside it (`K-VER-003`).
pub const KERNEL_VERSION: u32 = 1;

/// The deepest a node may sit below its unit's body, counted in
/// [`crate::Node::children`] steps (`K-DOC-006`). A block and each statement
/// in it are two levels.
pub const MAX_NESTING_DEPTH: usize = 64;

/// A workflow document: `main`, the declared functions, the entries a host
/// may start, the private bindings and the requirements manifest.
///
/// A document holds behaviour only. Where its nodes came from in authored
/// source is an optional [`crate::SourceMap`], and what its steps are called
/// is an optional [`crate::Captions`]; both are stored apart and keyed to
/// the document's [`DocumentId`].
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Document {
    pub manifest: Manifest,
    /// The declared functions, by name.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub functions: BTreeMap<Name, Function>,
    /// The declared functions a host may start, each with the typed
    /// signature and calling convention it is started under.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub entries: BTreeMap<Name, Entry>,
    /// The variables of `main` that are the program's own. Every other
    /// variable `main` declares at its top level is a session binding.
    #[serde(default, skip_serializing_if = "BTreeSet::is_empty")]
    pub private_bindings: BTreeSet<Name>,
    pub main: Block,
}

/// A declared entry's host signature and synchronous calling convention.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Entry {
    pub signature: Signature,
    pub convention: crate::StartConvention,
}

/// What a document needs from the environment that runs it. Admission
/// refuses a document whose requirements the environment does not meet.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Manifest {
    /// The kernel version the document is written in.
    pub kernel: u32,
    /// Every operation the document performs, or a library body it reaches
    /// performs, with what the code requires of it (`K-DOC-002`).
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub effects: BTreeMap<EffectName, OperationRequirement>,
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

/// The document's run-count ordinals by site: statement and block
/// starts, then action issues, then while-condition tests.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Ordinals {
    /// [`Document::site_ordinals`].
    pub sites: BTreeMap<Site, u32>,
    /// [`Document::action_ordinals`].
    pub actions: BTreeMap<Site, u32>,
    /// [`Document::condition_ordinals`].
    pub conditions: BTreeMap<Site, u32>,
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

    /// Every statement and block of this document's own units with the
    /// node itself, in [`Site`] order. A node's index in the answer is its
    /// ordinal: [`site_ordinals`](Self::site_ordinals) ranks this walk's
    /// sites alone. Expressions and actions are walked through but appear
    /// nowhere in it.
    pub fn ordinal_nodes(&self) -> Vec<(Site, Node<'_>)> {
        self.walk_sites().0
    }

    /// Dense ordinals for every statement and block in this document's own
    /// units, in [`Site`] order. Expressions and actions have no ordinal;
    /// closure bodies do. The rank depends only on the document, never on
    /// an executable's layout or the library it is compiled against.
    pub fn site_ordinals(&self) -> BTreeMap<Site, u32> {
        self.ordinals().sites
    }

    /// Action issue counters share the run-count channel with statement starts.
    /// They follow the statement/block ordinals, in Site order, so existing
    /// statement and replay coordinates keep their meaning.
    pub fn action_ordinals(&self) -> BTreeMap<Site, u32> {
        self.ordinals().actions
    }

    /// While-condition evaluations, separate from statement starts and body
    /// entries. Each test is counted before evaluation, including the last false test.
    pub fn condition_ordinals(&self) -> BTreeMap<Site, u32> {
        self.ordinals().conditions
    }

    /// [`site_ordinals`](Self::site_ordinals),
    /// [`action_ordinals`](Self::action_ordinals) and
    /// [`condition_ordinals`](Self::condition_ordinals) from one walk.
    pub fn ordinals(&self) -> Ordinals {
        let (nodes, actions) = self.walk_sites();
        let action_base = nodes.len();
        let condition_base = action_base + actions.len();
        let conditions = nodes
            .iter()
            .filter(|(_, node)| matches!(node, Node::Stmt(crate::Stmt::While { .. })))
            .enumerate()
            .map(|(i, (site, _))| (site.clone(), (condition_base + i) as u32))
            .collect();
        let actions = actions
            .into_iter()
            .enumerate()
            .map(|(i, site)| (site, (action_base + i) as u32))
            .collect();
        let sites = nodes
            .into_iter()
            .enumerate()
            .map(|(ordinal, (site, _))| (site, ordinal as u32))
            .collect();
        Ordinals {
            sites,
            actions,
            conditions,
        }
    }

    /// One pre-order walk of the document's own units, which visits sites
    /// in [`Site`] order: every statement and block with the node, and
    /// every action's site.
    fn walk_sites(&self) -> (Vec<(Site, Node<'_>)>, Vec<Site>) {
        let mut nodes = Vec::new();
        let mut actions = Vec::new();
        let units = std::iter::once((lash_vocabulary::sites::Unit::Main, &self.main)).chain(
            self.functions.iter().map(|(name, function)| {
                (
                    lash_vocabulary::sites::Unit::Function(name.clone()),
                    &function.body,
                )
            }),
        );
        let mut path = Vec::new();
        let mut pending = Vec::new();
        for (unit, body) in units {
            // Each entry is a node, its depth and its index in its parent;
            // `path` holds the walk's current site below the unit.
            pending.push((Node::Block(body), 0usize, 0u32));
            while let Some((node, depth, index)) = pending.pop() {
                path.truncate(depth.saturating_sub(1));
                if depth > 0 {
                    path.push(index);
                }
                let site = || Site::new(unit.clone(), path.clone());
                match node {
                    Node::Block(_) | Node::Stmt(_) => nodes.push((site(), node)),
                    Node::Action(_) | Node::Expr(crate::Expr::Act(_)) => actions.push(site()),
                    _ => {}
                }
                let children = node.children();
                pending.extend(
                    children
                        .into_iter()
                        .enumerate()
                        .rev()
                        .map(|(index, child)| (child, depth + 1, index as u32)),
                );
            }
        }
        (nodes, actions)
    }

    /// The document's behavioural identity (`K-ID-001`): the digest of its
    /// canonical bytes, which is the checkpoint graph's digest of its
    /// logical body.
    pub fn identity(&self) -> DocumentId {
        DocumentId::from_bytes(logical_digest(&self.canonical_bytes()))
    }

    /// The document's canonical encoding (`K-ID-003`): its logical token
    /// body. Two documents are equal exactly when these bytes are.
    pub fn canonical_bytes(&self) -> Vec<u8> {
        encode_body(self)
    }

    /// The host's readable form (`K-DOC-004`). Lash's own components pass
    /// a document as its token body ([`crate::IdentifiedDocument`]).
    pub fn to_json(&self) -> Result<String, EncodeError> {
        crate::document_work::note(|work| work.json_encodes += 1);
        to_json(self)
    }

    pub fn from_json(text: &str) -> Result<Self, DecodeError> {
        Self::from_json_with_limits(
            text,
            lash_vocabulary::data_limits::KernelDataLimits::standard(),
        )
    }
    /// Read under the host's recursive JSON safety policy.
    pub fn from_json_with_limits(
        text: &str,
        limits: lash_vocabulary::data_limits::KernelDataLimits,
    ) -> Result<Self, DecodeError> {
        crate::document_work::note(|work| work.json_decodes += 1);
        from_versioned_json_with_limits(text, limits)
    }
}

/// A value that could not be written as JSON. No type of this crate fails.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
#[error("cannot encode as JSON: {message}")]
pub struct EncodeError {
    pub message: String,
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

pub(crate) fn from_json_with_limits<T: DeserializeOwned>(
    text: &str,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<T, DecodeError> {
    let limit = limits.document_json_depth();
    let depth = json_depth(text);
    if depth > limit {
        return Err(DecodeError::TooDeep { depth, limit });
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

/// The deepest nesting of arrays and objects in a JSON text, counted without
/// recursion so that an over-deep text is refused before it is decoded.
fn json_depth(text: &str) -> usize {
    let mut depth = 0usize;
    let mut deepest = 0usize;
    let mut in_string = false;
    let mut escaped = false;
    for byte in text.bytes() {
        if in_string {
            match byte {
                _ if escaped => escaped = false,
                b'\\' => escaped = true,
                b'"' => in_string = false,
                _ => {}
            }
            continue;
        }
        match byte {
            b'"' => in_string = true,
            b'{' | b'[' => {
                depth += 1;
                deepest = deepest.max(depth);
            }
            b'}' | b']' => depth = depth.saturating_sub(1),
            _ => {}
        }
    }
    deepest
}

/// Read only the version envelope before decoding any forms (K-VER-003).
/// Unknown fields are skipped by serde, so a payload malformed for this
/// build cannot disguise an unsupported version as a form decoding fault.
pub(crate) fn from_versioned_json_with_limits<T: DeserializeOwned>(
    text: &str,
    limits: lash_vocabulary::data_limits::KernelDataLimits,
) -> Result<T, DecodeError> {
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
    let header: Header = from_json_with_limits(text, limits)?;
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
    from_json_with_limits(text, limits)
}

/// Linked document model; implementation belongs to pkg-doc.
pub mod linked {
    use crate::function::linked::FunctionDefinition;
    use crate::function_value::linked::StartConvention;
    use crate::{Block, NumberPolicy};
    use lash_vocabulary::{claims::NodeRequirements, names::*, types::*};
    use serde::{Deserialize, Serialize};
    use std::collections::{BTreeMap, BTreeSet};
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    pub struct Document {
        pub imports: BTreeMap<ImportSlot, Import>,
        pub exports: BTreeMap<Name, Export>,
        pub main: Option<Block>,
        pub functions: BTreeMap<FunctionId, FunctionDefinition>,
        pub requires: NodeRequirements,
        pub numbers: NumberPolicy,
        pub private_bindings: BTreeSet<Name>,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(
        tag = "kind",
        content = "value",
        rename_all = "snake_case",
        deny_unknown_fields
    )]
    pub enum Export {
        Function(FunctionExport),
        Type(TypeDef),
        Error { kind: ErrorKind },
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    /// Public call signature, settled result and defining convention. Startability is exactly function kind plus data_only.
    pub struct FunctionExport {
        pub function: FunctionId,
        pub signature: Signature,
        pub convention: StartConvention,
        pub asynchronous: bool,
        pub rest: bool,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(deny_unknown_fields)]
    /// A Brand paired with representation, executable checker and named members; validated together at admission.
    pub struct TypeDef {
        pub brand: Brand,
        pub representation: Type,
        pub checker: FunctionId,
        pub members: BTreeMap<Segment, TypeMember>,
    }
    #[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
    #[serde(
        tag = "kind",
        content = "value",
        rename_all = "snake_case",
        deny_unknown_fields
    )]
    /// A static function, explicit-receiver method, or property in its owning TypeDef.
    pub enum TypeMember {
        Static(FunctionExport),
        Method(FunctionExport),
        Property(FunctionExport),
    }
}
