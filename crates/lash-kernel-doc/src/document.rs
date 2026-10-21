//! The workflow document: a kernel program and what it requires.

use schemars::JsonSchema;
use serde::de::DeserializeOwned;

use crate::ast::{Block, Node};
use crate::graph_tokens::{encode_body, logical_digest};
use crate::name::DocumentId;
use lash_vocabulary::sites::Site;

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

use crate::NumberPolicy;
use crate::function::FunctionDefinition;
use crate::function_value::StartConvention;
use lash_vocabulary::{claims::NodeRequirements, names::*, types::*};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// Public call signature, settled result and defining convention. Startability is exactly function kind plus data_only.
pub struct FunctionExport {
    pub function: FunctionId,
    pub signature: Signature,
    pub convention: StartConvention,
    pub asynchronous: bool,
    pub rest: bool,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
/// A Brand paired with representation, executable checker and named members; validated together at admission.
pub struct TypeDef {
    pub brand: Brand,
    pub representation: Type,
    pub checker: FunctionId,
    pub members: BTreeMap<Segment, TypeMember>,
}
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
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
            imports: BTreeMap::new(),
            exports: BTreeMap::new(),
            functions: BTreeMap::new(),
            main: Some(main),
            requires: NodeRequirements {
                kernels: BTreeSet::from([KERNEL_VERSION]),
                ..NodeRequirements::default()
            },
            numbers,
            private_bindings: BTreeSet::new(),
        }
    }

    /// Executable bodies and their stable semantic coordinates.
    pub fn units(&self) -> impl Iterator<Item = (crate::Unit, &Block)> {
        self.main
            .iter()
            .map(|body| (crate::Unit::Main, body))
            .chain(self.functions.iter().filter_map(|(id, function)| {
                function
                    .body()
                    .map(|body| (crate::Unit::Library(*id), &body.block))
            }))
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
        let units = self.units();
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
        Self::from_json_with_limits(text, crate::KernelDataLimits::standard())
    }
    /// Read under the host's recursive JSON safety policy.
    pub fn from_json_with_limits(
        text: &str,
        limits: crate::KernelDataLimits,
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
    limits: crate::KernelDataLimits,
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
    limits: crate::KernelDataLimits,
) -> Result<T, DecodeError> {
    #[derive(Deserialize)]
    struct Header {
        #[serde(default)]
        kernel: Option<u32>,
        #[serde(default)]
        requires: Option<lash_vocabulary::claims::NodeRequirements>,
    }
    let header: Header = from_json_with_limits(text, limits)?;
    let kernels = header
        .requires
        .map(|requires| requires.kernels)
        .unwrap_or_default();
    if let Some(found) = kernels
        .into_iter()
        .chain(header.kernel)
        .find(|kernel| crate::KernelVersion::of(*kernel).is_none())
    {
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

impl TypeMember {
    pub fn function(&self) -> &FunctionExport {
        match self {
            Self::Static(f) | Self::Method(f) | Self::Property(f) => f,
        }
    }
}
impl Document {
    pub fn function_names(&self) -> BTreeMap<FunctionId, FunctionName> {
        self.functions
            .iter()
            .map(|(id, f)| (*id, f.name.clone()))
            .collect()
    }
}

impl Document {
    /// Re-identify edited local definitions bottom-up, carrying references by identity.
    /// Cross-document Imports are unchanged. Lexical recursion is not a content-hash cycle.
    pub fn reidentify_functions(&mut self) -> Result<BTreeMap<FunctionId, FunctionId>, String> {
        fn one(
            id: FunctionId,
            source: &BTreeMap<FunctionId, FunctionDefinition>,
            visiting: &mut BTreeSet<FunctionId>,
            done: &mut BTreeMap<FunctionId, FunctionId>,
            out: &mut BTreeMap<FunctionId, FunctionDefinition>,
        ) -> Result<(), String> {
            if done.contains_key(&id) {
                return Ok(());
            }
            if !visiting.insert(id) {
                return Err(format!("cyclic local FunctionId references at {id}"));
            }
            let mut function = source
                .get(&id)
                .ok_or_else(|| format!("missing local function {id}"))?
                .clone();
            if let Some(body) = function.body_mut() {
                let dependencies = crate::Node::Block(&body.block).functions_within();
                for dependency in dependencies {
                    one(dependency, source, visiting, done, out)?;
                }
                let _: Result<(), std::convert::Infallible> =
                    crate::NodeMut::Block(&mut body.block).try_each_function_mut(
                        crate::Site::new(crate::Unit::Library(id), []),
                        &mut |_, reference| {
                            if let Some(new) = done.get(reference) {
                                *reference = *new;
                            }
                            Ok(())
                        },
                    );
                body.functions = std::mem::take(&mut body.functions)
                    .into_iter()
                    .map(|(old, name)| (done.get(&old).copied().unwrap_or(old), name))
                    .collect();
            }
            let new = function.identity();
            out.insert(new, function);
            done.insert(id, new);
            visiting.remove(&id);
            Ok(())
        }
        let mut done = BTreeMap::new();
        let mut out = BTreeMap::new();
        let mut visiting = BTreeSet::new();
        for id in self.functions.keys() {
            one(*id, &self.functions, &mut visiting, &mut done, &mut out)?;
        }
        if let Some(main) = &mut self.main {
            let _: Result<(), std::convert::Infallible> = crate::NodeMut::Block(main)
                .try_each_function_mut(crate::Site::new(crate::Unit::Main, []), &mut |_, id| {
                    if let Some(new) = done.get(id) {
                        *id = *new;
                    }
                    Ok(())
                });
        }
        fn export(function: &mut FunctionExport, done: &BTreeMap<FunctionId, FunctionId>) {
            if let Some(new) = done.get(&function.function) {
                function.function = *new;
            }
            let targets = match &mut function.convention {
                StartConvention::Direct => vec![],
                StartConvention::TsDirect { await_fn } => vec![await_fn],
                StartConvention::TsArgsList { await_fn, same_fn } => vec![await_fn, same_fn],
                StartConvention::PyDirect { cleanup } => vec![cleanup],
            };
            for target in targets {
                if let crate::FunctionTarget::Local(id) = target
                    && let Some(new) = done.get(id)
                {
                    *id = *new;
                }
            }
        }
        for offered in self.exports.values_mut() {
            match offered {
                Export::Function(f) => export(f, &done),
                Export::Type(ty) => {
                    if let Some(new) = done.get(&ty.checker) {
                        ty.checker = *new;
                    }
                    for member in ty.members.values_mut() {
                        match member {
                            TypeMember::Static(f)
                            | TypeMember::Method(f)
                            | TypeMember::Property(f) => export(f, &done),
                        }
                    }
                }
                Export::Error { .. } => {}
            }
        }
        self.requires.natives = self
            .requires
            .natives
            .iter()
            .map(|id| done.get(id).copied().unwrap_or(*id))
            .collect();
        self.functions = out;
        Ok(done)
    }
}
impl Document {
    /// The singleton kernel version; structural validation establishes this invariant.
    pub fn kernel(&self) -> u32 {
        self.requires
            .kernels
            .iter()
            .next()
            .copied()
            .unwrap_or_else(|| unreachable!("validated Document has one kernel"))
    }
}
