//! A session's execution state as a checkpoint graph (FIG-5887 §3, §4, §6).
//!
//! The state is one SessionExecution node: the dialect and number policy,
//! the kernel version its values are written in, every data binding with
//! the one local table of heap objects they reach, the alias table that
//! binds each name holding a saved function to a member of an immutable
//! [`Bundle`], the bindings that were not carried with why, and the cell
//! count. Large text and bytes anywhere in it are shared leaves, and a
//! bundle is shared by every state that holds it; nothing else is.
//!
//! Every capture writes the complete state. Object identities are the
//! session's own heap bookkeeping, renumbered as ever
//! ([`SessionBindings::adopt`](super::SessionBindings)), never a storage
//! identity: the root holds no slot table and names no earlier root.

use std::collections::{BTreeMap, BTreeSet};

use lash_core_store::checkpoint_graph::{
    DecodeLimits, EncodeBudget, EncodedNode, ExplicitLimit, GraphDecoder, GraphRefusal,
    GraphStaging, NodeId, NodeKind, TypedBody,
};
use lash_kernel_dialect::{Bundle, BundleBody, NotSaved};
use lash_kernel_doc::graph_tokens::{
    ChildRef, TokenCodec, TokenError, TokenReader, TokenWriter, check_ascending,
};
use lash_kernel_doc::{Document, Name, NumberPolicy, Object, ObjectId, Value};

use super::{CodeModeSnapshotError, HeldFunction, SessionBindings, bound};

/// A SessionExecution node's logical body.
#[derive(Clone, Debug, PartialEq)]
pub(super) struct SessionExecutionBody {
    pub dialect: String,
    pub numbers: NumberPolicy,
    /// The kernel version the bindings' values are written in.
    pub kernel: u32,
    pub bindings: BTreeMap<Name, Value>,
    pub objects: BTreeMap<ObjectId, Object>,
    /// One row per binding that holds a saved function, sorted by binding.
    pub aliases: Vec<AliasRow>,
    pub not_carried: BTreeMap<Name, NotSaved>,
    pub cells: u64,
}

/// A binding that holds a saved function: the bundle, the member it holds,
/// and the generation that froze it, the session's cell counted from one,
/// or none for a function the session was created with.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(super) struct AliasRow {
    pub binding: Name,
    pub bundle: ChildRef,
    pub member: u64,
    pub generation: Option<u64>,
}

impl TokenCodec for SessionExecutionBody {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(8);
        out.text(&self.dialect);
        self.numbers.encode(out);
        self.kernel.encode(out);
        self.bindings.encode(out);
        self.objects.encode(out);
        self.aliases.encode(out);
        self.not_carried.encode(out);
        self.cells.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(8)?;
        let body = Self {
            dialect: String::decode(input)?,
            numbers: NumberPolicy::decode(input)?,
            kernel: u32::decode(input)?,
            bindings: BTreeMap::decode(input)?,
            objects: BTreeMap::decode(input)?,
            aliases: Vec::decode(input)?,
            not_carried: BTreeMap::decode(input)?,
            cells: u64::decode(input)?,
        };
        check_ascending(
            &body
                .aliases
                .iter()
                .map(|row| &row.binding)
                .collect::<Vec<_>>(),
        )?;
        Ok(body)
    }
}

impl TokenCodec for AliasRow {
    fn encode(&self, out: &mut TokenWriter) {
        out.seq(4);
        self.binding.encode(out);
        self.bundle.encode(out);
        self.member.encode(out);
        self.generation.encode(out);
    }

    fn decode(input: &mut TokenReader<'_>) -> Result<Self, TokenError> {
        input.expect_struct(4)?;
        Ok(Self {
            binding: Name::decode(input)?,
            bundle: ChildRef::decode(input)?,
            member: u64::decode(input)?,
            generation: Option::decode(input)?,
        })
    }
}

/// A staging area for one capture. The commit that installs it admits its
/// size against the host's commit budget (FIG-5887 §9); the encoder itself
/// sets no second, silent bound.
pub(super) fn staging() -> GraphStaging {
    GraphStaging::new(EncodeBudget::new(ExplicitLimit::new(u64::MAX)))
}

/// Stages a node's logical body and answers its reference.
pub(super) fn stage_node(
    staging: &mut GraphStaging,
    kind: u32,
    logical: &[u8],
) -> Result<ChildRef, GraphRefusal> {
    staging
        .encode(NodeKind(kind), TypedBody::logical(logical))
        .map(|id| id.child_ref())
}

/// Stages the complete state of `bindings` and answers its SessionExecution
/// root. A bundle names its release manifest, which is the release pin's
/// and never staged here ([`super::release`]).
pub(super) fn stage(
    dialect: &str,
    numbers: NumberPolicy,
    bindings: &SessionBindings,
    staging: &mut GraphStaging,
) -> Result<NodeId, GraphRefusal> {
    let functions = bindings.functions();
    let mut aliases = Vec::new();
    for (declared, held) in &bindings.functions {
        let bundle = Bundle::of(&functions, declared).ok_or(GraphRefusal::InvalidBody {
            kind: NodeKind::FUNCTION_BUNDLE.0,
            reason: format!("the session holds no function `{declared}`"),
        })?;
        let manifest = super::release::manifest(&bundle)?;
        let reference =
            bundle.stage(manifest, |kind, logical| stage_node(staging, kind, logical))?;
        let member = bundle
            .index(declared)
            .ok_or_else(|| GraphRefusal::InvalidBody {
                kind: NodeKind::FUNCTION_BUNDLE.0,
                reason: format!("`{declared}` is not a member of its own bundle"),
            })?;
        for binding in std::iter::once(declared).chain(&held.aliases) {
            aliases.push(AliasRow {
                binding: binding.clone(),
                bundle: reference,
                member,
                generation: held.cell,
            });
        }
    }
    aliases.sort_by(|left, right| left.binding.cmp(&right.binding));
    let body = SessionExecutionBody {
        dialect: dialect.to_owned(),
        numbers,
        kernel: lash_kernel_doc::KERNEL_VERSION,
        bindings: bindings.variables.clone(),
        objects: bindings.objects.clone(),
        aliases,
        not_carried: bindings.not_carried.clone(),
        cells: bindings.cells,
    };
    staging.encode_value(NodeKind::SESSION_EXECUTION, &body)
}

/// What a stored state holds, before this build carries its saved
/// functions forward.
pub(super) struct Read {
    pub dialect: String,
    pub numbers: NumberPolicy,
    pub kernel: u32,
    pub bindings: SessionBindings,
}

/// How many times over a read may touch what was supplied. A node shared
/// by many references (a leaf two bindings hold, a document two bundles
/// hold) is read once per reference, so a read's bytes exceed what was
/// supplied; the allowance bounds that work, and the bytes a state's
/// bodies spell out, to a fixed multiple of the supplied graph.
const REREAD_ALLOWANCE: u64 = 64;

/// Limits for reading a supplied graph: the format's own node and depth
/// bounds, and [`REREAD_ALLOWANCE`] times what was supplied, with the
/// release manifests this build holds. The store that hydrated the graph
/// bounded what it supplied.
fn limits(source: &super::release::WithHeld<'_>) -> DecodeLimits {
    let supplied = source.bytes();
    let bound = supplied.saturating_mul(REREAD_ALLOWANCE);
    DecodeLimits {
        total_bytes: bound,
        nodes: bound,
        logical_bytes: bound,
        ..DecodeLimits::within_total(supplied)
    }
}

/// Reads the state rooted at `root` from `nodes`. Every node is checked:
/// its digest, its canonical form and its kind. Each bundle must be
/// declared by its members' documents, name a release manifest, and agree
/// with every other bundle on each function the session holds under a
/// name.
pub(super) fn read(
    root: NodeId,
    nodes: &BTreeMap<NodeId, EncodedNode>,
) -> Result<Read, CodeModeSnapshotError> {
    let source = super::release::WithHeld::new(nodes);
    let mut decoder = GraphDecoder::new(&source, limits(&source));
    let body: SessionExecutionBody = decoder.decode(root, NodeKind::SESSION_EXECUTION)?;
    let mut bundles: BTreeMap<ChildRef, Bundle> = BTreeMap::new();
    let mut documents: BTreeMap<ChildRef, Document> = BTreeMap::new();
    for row in &body.aliases {
        if bundles.contains_key(&row.bundle) {
            continue;
        }
        let id = NodeId::from(row.bundle);
        let bundle_body: BundleBody = decoder.decode(id, NodeKind::FUNCTION_BUNDLE)?;
        let (bundle, manifest) = Bundle::assemble(bundle_body, |reference| {
            if let Some(document) = documents.get(&reference) {
                return Ok(document.clone());
            }
            let document: Document = decoder.decode(NodeId::from(reference), NodeKind::DOCUMENT)?;
            documents.insert(reference, document.clone());
            Ok::<_, CodeModeSnapshotError>(document)
        })?;
        super::release::check(&mut decoder, NodeId::from(manifest))?;
        bundles.insert(row.bundle, bundle);
    }
    let incoherent = |details: String| CodeModeSnapshotError::FormatMismatch { details };
    // The rows of one function: its bundle and member, and every binding
    // that holds it.
    let mut held: BTreeMap<(ChildRef, u64), Vec<&AliasRow>> = BTreeMap::new();
    for row in &body.aliases {
        held.entry((row.bundle, row.member)).or_default().push(row);
    }
    let mut functions: BTreeMap<Name, HeldFunction> = BTreeMap::new();
    for ((bundle, member), rows) in held {
        let bundle = &bundles[&bundle];
        let function = bundle.member(member).ok_or_else(|| {
            incoherent(format!(
                "`{}` holds member {member} of a bundle of {}",
                rows[0].binding,
                bundle.members().len()
            ))
        })?;
        let generation = rows[0].generation;
        if rows.iter().any(|row| row.generation != generation) {
            return Err(incoherent(format!(
                "the bindings of `{}` hold it in different generations",
                function.name
            )));
        }
        let mut aliases: BTreeSet<Name> = rows.iter().map(|row| row.binding.clone()).collect();
        if !aliases.remove(&function.name) {
            return Err(incoherent(format!(
                "no binding holds `{}` under the name it is declared under",
                function.name
            )));
        }
        if functions.contains_key(&function.name) {
            return Err(incoherent(format!(
                "`{}` is declared by two functions",
                function.name
            )));
        }
        functions.insert(
            function.name.clone(),
            HeldFunction {
                function: function.clone(),
                cell: generation,
                aliases,
            },
        );
    }
    // A bundle's member is the function the session holds under its name,
    // where the session holds one: a cell installs what the session holds.
    for bundle in bundles.values() {
        for (name, member) in bundle.members() {
            if let Some(held) = functions.get(name)
                && held.function != *member
            {
                return Err(incoherent(format!(
                    "a bundle holds a `{name}` other than the session's"
                )));
            }
        }
    }
    super::check_held(&functions, &body.bindings).map_err(incoherent)?;
    super::check_reached(&body.bindings, &body.objects).map_err(incoherent)?;
    if let Some(name) = bound(&functions)
        .into_keys()
        .chain(body.bindings.keys())
        .find(|name| body.not_carried.contains_key(*name))
    {
        return Err(incoherent(format!(
            "`{name}` is both bound and listed as not carried"
        )));
    }
    // The stored numbering stands, so an unchanged state captures the root
    // it was read from; the next change renumbers every binding afresh.
    let bindings = SessionBindings {
        variables: body.bindings,
        objects: body.objects,
        not_carried: body.not_carried,
        functions,
        cells: body.cells,
        ..SessionBindings::default()
    };
    Ok(Read {
        dialect: body.dialect,
        numbers: body.numbers,
        kernel: body.kernel,
        bindings,
    })
}
