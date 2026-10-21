//! Readable export by region, shared by the dialects (FIG-6953).
//!
//! Authored source is returned only when lowering it reproduces the complete
//! document and captions. A refusal never exposes a private kernel codec.

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
    for import in document.imports.values() {
        let name = &import.name;
        let kind = match &import.kind {
            ImportKind::Operation { signature } => {
                crate::NamespaceKind::Operation(crate::OperationSignature {
                    signature: signature.clone(),
                    controls: lash_vocabulary::TurnControls::none(),
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

    /// The module of `functions` and the lifted `main`, when `main` lifted.
    fn whole(&self, functions: &BTreeSet<Name>) -> Option<Whole>;

    /// `source` read back against the printed document. A refusal gives
    /// the offset it points at, if any.
    ///
    /// # Errors
    ///
    /// The front end's refusal.
    fn read(&self, source: &str) -> Result<RegionRead, Option<usize>>;
}

/// Print a whole authored module only when reading it reproduces the document
/// and captions exactly.
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
    let whole = regions.whole(&lifted).ok_or_else(|| {
        PrintError::Invalid("the document cannot be expressed as authored source".to_owned())
    })?;
    if whole.source.len() > options.limits.max_bytes {
        return Err(PrintError::Bytes {
            limit: options.limits.max_bytes,
        });
    }
    let read = regions.read(&whole.source).map_err(|_| {
        PrintError::Invalid("authored source does not read as this document".to_owned())
    })?;
    if !read.matches(document, captions) {
        return Err(PrintError::Invalid(
            "authored source changes the document".to_owned(),
        ));
    }
    whole_printed(document, whole.source, read.map, regions.format(), options)
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

/// Source declaration order, projected from the bindings authored lowering
/// creates. Other executable top-level work must be lifted as cell code.
pub fn export_order(document: &k::Document) -> Option<Vec<Name>> {
    let mut order = Vec::new();
    if document.exports.is_empty() {
        return Some(order);
    }
    for statement in document.main.as_deref().unwrap_or_default() {
        let (name, id) = match statement {
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Function(id))),
            } => (name, id),
            k::Stmt::Assign {
                place: k::Place::Variable(name),
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Function(id))),
                ..
            } => (name, id),
            k::Stmt::Let {
                name,
                value: k::Rhs::Expr(k::Expr::Literal(k::Literal::Absent)),
            } if document.exports.contains_key(name) => continue,
            _ => return None,
        };
        if !matches!(document.exports.get(name),Some(k::Export::Function(f)) if f.function==*id)
            || order.contains(name)
        {
            return None;
        }
        order.push(name.clone());
    }
    (order.len() == document.exports.len()).then_some(order)
}
