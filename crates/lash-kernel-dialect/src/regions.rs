//! Readable export by region, shared by the dialects (FIG-6953).
//!
//! Authored source is returned only when lowering it reproduces the complete
//! document and every spelled caption; unspellable targets are reported dropped. A refusal never exposes a private kernel codec.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use lash_kernel_doc::{self as k, FunctionCatalog, SourceMap};

use lash_vocabulary::{FunctionId, FunctionName, Name};

use crate::{NamespaceRefusal, Namespaces, PrintError, PrintOptions, PrintedSource, SourceFormat};

/// The functions a document lists: those it calls and those their bodies
/// reach (`K-DOC-002`).
pub fn reachable(
    used: BTreeMap<FunctionId, FunctionName>,
    library: &dyn crate::Library,
) -> BTreeMap<FunctionId, FunctionName> {
    let mut listed = BTreeMap::new();
    let mut pending: Vec<(FunctionId, FunctionName)> = used.into_iter().collect();
    while let Some((function, name)) = pending.pop() {
        let std::collections::btree_map::Entry::Vacant(entry) = listed.entry(function) else {
            continue;
        };
        entry.insert(name);
        let catalog: &dyn FunctionCatalog = library;
        // A registered function lists its callees without being decoded.
        if let Some(registered) = catalog.registered(&function) {
            pending.extend(registered.listed());
        } else if let Some(body) = catalog
            .definition(&function)
            .and_then(|definition| definition.body())
        {
            // An identity digests its function's name, so a function already
            // listed is listed by the name any other body calls it by.
            pending.extend(
                body.functions
                    .iter()
                    .filter(|(function, _)| !listed.contains_key(*function))
                    .map(|(function, name)| (*function, name.clone())),
            );
        }
    }
    listed
}

/// Reconstruct source-name metadata from signature-only Imports. This never
/// fulfils a Link or queries providers; source reading keeps the base's slots.
pub fn namespaces(document: &k::Document) -> Result<Namespaces, NamespaceRefusal> {
    use lash_vocabulary::{ImportKind, package::Visibility};
    let mut packages = BTreeMap::<
        lash_vocabulary::PackagePath,
        BTreeMap<lash_vocabulary::Segment, crate::NamespaceMember>,
    >::new();
    let terminal = terminal_operations(document);
    for (slot, import) in &document.imports {
        let name = &import.name;
        let kind = match &import.kind {
            ImportKind::Operation { signature } => {
                crate::NamespaceKind::Operation(crate::OperationSignature {
                    signature: signature.clone(),
                    // Both host control kinds lower to this same terminal
                    // wrapper. Reconstruct its lowering fact, not the host's
                    // declaration (which is outside the document).
                    controls: if terminal.contains(slot) {
                        lash_vocabulary::TurnControls::finish(lash_vocabulary::JsonSchema::any())
                    } else {
                        lash_vocabulary::TurnControls::none()
                    },
                    mode: lash_vocabulary::OperationMode::Waited,
                })
            }
            ImportKind::Code {
                signature,
                convention,
            } => crate::NamespaceKind::Function(crate::FunctionMember {
                name: name.clone(),
                signature: signature.clone(),
                convention: *convention,
                asynchronous: false,
                rest: false,
            }),
            ImportKind::Type { .. } => crate::NamespaceKind::Type(crate::ValueType {
                representation: lash_vocabulary::Type::Any,
                members: BTreeMap::new(),
            }),
            ImportKind::Error { .. } => crate::NamespaceKind::Error,
        };
        packages.entry(name.package.clone()).or_default().insert(
            name.member.clone(),
            crate::NamespaceMember {
                visibility: Visibility::Public,
                kind,
            },
        );
    }
    Namespaces::new(packages, BTreeSet::new(), &|_| false)
}

/// The exact terminal-call shape authored lowering emits, keyed by the
/// owning unit's import contract and projected onto matching root imports.
/// A local slot or display name alone never supplies a control fact.
fn terminal_operations(document: &k::Document) -> BTreeSet<lash_vocabulary::ImportSlot> {
    let mut terminal = BTreeSet::new();
    let mut roots = BTreeMap::<_, Vec<_>>::new();
    for (slot, import) in &document.imports {
        roots.entry(&import.name).or_default().push((*slot, import));
    }
    for (unit, block) in document.units() {
        let imports = match unit {
            lash_vocabulary::sites::Unit::Main | lash_vocabulary::sites::Unit::Function(_) => {
                &document.imports
            }
            lash_vocabulary::sites::Unit::Library(function) => {
                let Some(body) = document.functions.get(&function).and_then(|f| f.body()) else {
                    continue;
                };
                &body.imports
            }
        };
        let mut pending = vec![k::Node::Block(block)];
        while let Some(node) = pending.pop() {
            if let k::Node::Stmt(k::Stmt::Finish {
                value: k::Expr::Let { value, body, .. },
            }) = node
                && matches!(&**body, k::Expr::Literal(k::Literal::Null))
                && let k::Expr::Act(action) = &**value
                && let k::ActKind::Perform { slot, .. } = action.kind
                && let Some(import) = imports.get(&slot)
            {
                terminal.extend(
                    roots
                        .get(&import.name)
                        .into_iter()
                        .flatten()
                        .filter_map(|(root, candidate)| (*candidate == import).then_some(*root)),
                );
            }
            pending.extend(node.children());
        }
    }
    terminal
}

/// A printed module, read back.
pub struct RegionRead {
    pub document: k::Document,
    pub captions: k::Captions,
    /// The front end's map of the lifted regions, at their offsets in the
    /// source read.
    pub map: Option<SourceMap>,
}

impl RegionRead {
    /// Whether this is `document` with `captions`.
    pub fn matches(&self, document: &k::Document, captions: &k::Captions) -> bool {
        self.document.canonical_bytes() == document.canonical_bytes()
            && self.captions.entries() == captions.entries()
    }
}

/// A module of lifted regions only, with where each lifted function is in
/// it.
pub struct Whole {
    pub source: String,
    pub functions: BTreeMap<Name, Range<usize>>,
}

/// What a dialect prints and reads for [`print_verified`].
pub trait Regions {
    /// The source language and format printed.
    fn format(&self) -> SourceFormat;

    /// The module of `functions` and the lifted `main`.
    ///
    /// # Errors
    /// A typed refusal at an unspellable body, or a source limit.
    fn whole(&self, functions: &BTreeSet<Name>) -> Result<Whole, PrintError>;

    /// `source` read back against the printed document. A refusal gives
    /// the offset it points at, if any.
    ///
    /// # Errors
    ///
    /// The front end's refusal.
    fn read(&self, source: &str) -> Result<RegionRead, Option<usize>>;
}

/// Print a whole authored module only when reading it reproduces the document
/// and every spelled caption exactly. Report unspellable targets as dropped.
///
/// # Errors
/// Refuses unrepresentable documents, unequal lowering identities and source
/// exceeding the configured limits.
pub fn print_verified(
    document: &k::Document,
    captions: Option<&k::Captions>,
    options: PrintOptions,
    lifted: BTreeSet<Name>,
    regions: &dyn Regions,
) -> Result<PrintedSource, PrintError> {
    let empty = k::Captions::empty(document.identity());
    let captions = captions.unwrap_or(&empty);
    crate::imperative::check_captions(document, captions, options.limits)?;
    let whole = regions.whole(&lifted)?;
    if whole.source.len() > options.limits.max_bytes {
        return Err(PrintError::Bytes {
            limit: options.limits.max_bytes,
        });
    }
    let unprintable = || PrintError::Unprintable {
        site: lash_vocabulary::sites::Site::new(lash_vocabulary::sites::Unit::Main, Vec::new()),
    };
    let read = regions.read(&whole.source).map_err(|_| unprintable())?;
    if read.document.canonical_bytes() != document.canonical_bytes()
        || read
            .captions
            .entries()
            .iter()
            .any(|entry| captions.get(&entry.target) != Some(&entry.caption))
    {
        return Err(unprintable());
    }
    // A retained library body or synthetic binding has no authored syntax.
    // Compare every caption that was spelled exactly, and report each input
    // target absent from the source rather than silently losing its sidecar.
    let dropped = captions
        .entries()
        .iter()
        .filter(|entry| read.captions.get(&entry.target).is_none())
        .map(|entry| entry.target.clone())
        .collect();
    let mut printed = whole_printed(document, whole.source, read.map, regions.format(), options)?;
    printed.dropped = dropped;
    Ok(printed)
}

/// The printed source of a module whose every region lifted.
fn whole_printed(
    document: &k::Document,
    source: String,
    map: Option<SourceMap>,
    format: SourceFormat,
    options: PrintOptions,
) -> Result<PrintedSource, PrintError> {
    let source_map = if options.source_map {
        let sites = map.map(|map| map.sites).unwrap_or_default();
        Some(
            SourceMap::new(document, &source, format.language(), sites)
                .map_err(|refused| PrintError::Invalid(refused.to_string()))?,
        )
    } else {
        None
    };
    Ok(PrintedSource {
        source,
        format,
        source_map,
        dropped: Vec::new(),
    })
}

/// Authored module parts in their Main statement order. Export bindings
/// supply declarations; other runs retain their original statement indices.
pub enum ModulePart {
    Export(Name),
    Statements(Range<usize>),
    /// A scope predeclaration restored by lowering the later export binding.
    Implicit,
}

/// Project module declarations and executable work from the same Main body.
/// Every Export must have exactly one matching binding; nested declarations
/// remain within their containing statement run.
pub fn module_parts(document: &k::Document) -> Option<Vec<ModulePart>> {
    let mut parts = Vec::new();
    let mut seen = BTreeSet::new();
    let main = document.main.as_deref().unwrap_or_default();
    let mut first = 0;
    for (index, statement) in main.iter().enumerate() {
        let part = match statement {
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Function(id))),
            }
            | k::Stmt::Assign {
                place: k::Place::Variable(name),
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Function(id))),
                ..
            } if matches!(document.exports.get(name), Some(k::Export::Function(f)) if f.function == *id) =>
            {
                if !seen.insert(name.clone()) {
                    return None;
                }
                ModulePart::Export(name.clone())
            }
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)),
            } if document.exports.contains_key(name) => ModulePart::Implicit,
            _ => continue,
        };
        if first < index {
            parts.push(ModulePart::Statements(first..index));
        }
        parts.push(part);
        first = index + 1;
    }
    if first < main.len() {
        parts.push(ModulePart::Statements(first..main.len()));
    }
    (seen.len() == document.exports.len()).then_some(parts)
}
