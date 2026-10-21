//! Readable export by region, shared by the dialects (FIG-6953).
//!
//! Each declared function of a document, and its `main`, is a region. A
//! dialect lifts each region back to the source an author writes; a lift is
//! kept only when reading the printed module back gives that region again.
//! A module whose every region lifts is a plain workflow module: lowering it
//! gives the document, with its identity, and needs no base. A region that
//! does not lift prints in its dialect's kernel surface, opened by the
//! dialect's kernel marker; such a module keeps the kernel layout, its
//! `main` in kernel form, and reads back against its base: the lifted
//! regions through the front end, the kernel-form ones through the exact
//! reader, assembled here. Printing never gives source that reads as
//! another document: when no mixture verifies, every region prints in
//! kernel form.

use std::collections::{BTreeMap, BTreeSet};
use std::ops::Range;

use lash_kernel_doc as k;
use lash_kernel_doc::FunctionCatalog;
use lash_kernel_doc::SourceLanguage;
use lash_kernel_doc::SourceMap;
use lash_vocabulary::legacy_names::FunctionId;
use lash_vocabulary::legacy_names::FunctionName;
use lash_vocabulary::legacy_names::Name;

use crate::EffectControl;
use crate::Library;
use crate::Lowered;
use crate::NamespaceRefusal;
use crate::Namespaces;
use crate::PrintError;
use crate::PrintOptions;
use crate::PrintedSource;
use crate::SourceFormat;
use crate::require_through_bodies;
use lash_vocabulary::names::QualifiedOperation;

/// The functions a document lists: those it calls and those their bodies
/// reach (`K-DOC-002`).
pub fn reachable(
    used: BTreeMap<FunctionId, FunctionName>,
    library: &dyn Library,
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

/// The operations `document` performs, by the paths source calls them
/// through: what lifted source of the document resolves against. A
/// control is one a `finish` of the document performs.
///
/// # Errors
///
/// When the document's operations compose no namespace.
pub fn namespaces(document: &k::Document) -> Result<Namespaces, NamespaceRefusal> {
    let requirements = document
        .manifest
        .effects
        .iter()
        .filter(|(effect, _)| QualifiedOperation::from_effect(effect).is_ok())
        .map(|(effect, requirement)| (effect.clone(), requirement.clone()))
        .collect::<BTreeMap<_, _>>();
    let controls = finished(document)
        .into_iter()
        .filter(|effect| requirements.contains_key(effect))
        .map(|effect| (effect, BTreeSet::from([EffectControl::Finish])))
        .collect();
    Namespaces::of_requirements(&requirements, &controls)
}

/// The effects a `finish` statement of `document` performs.
fn finished(document: &k::Document) -> BTreeSet<k::EffectName> {
    let mut effects = BTreeSet::new();
    let mut pending: Vec<k::Node<'_>> = std::iter::once(&document.main)
        .chain(document.functions.values().map(|function| &function.body))
        .map(|block| k::Node::Block(block))
        .collect();
    let mut within = Vec::new();
    while let Some(node) = pending.pop() {
        if let k::Node::Stmt(k::Stmt::Finish { value }) = node {
            within.push(k::Node::Expr(value));
            continue;
        }
        pending.extend(node.children());
    }
    while let Some(node) = within.pop() {
        if let k::Node::Expr(k::Expr::Act(act)) = node
            && let k::ActKind::Perform { effect, .. } = &act.kind
        {
            effects.insert(effect.clone());
        }
        within.extend(node.children());
    }
    effects
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

    /// Whether the declared function `name` read back as it is in
    /// `document`, with its signature and captions.
    pub fn region_matches(
        &self,
        name: &Name,
        document: &k::Document,
        captions: &k::Captions,
    ) -> bool {
        let of = |captions: &k::Captions| {
            captions
                .entries()
                .iter()
                .filter(|entry| unit(&entry.target) == Some(name))
                .cloned()
                .collect::<Vec<_>>()
        };
        self.document.functions.get(name) == document.functions.get(name)
            && self.document.entries.get(name) == document.entries.get(name)
            && of(&self.captions) == of(captions)
    }
}

/// The declared function a caption belongs to.
fn unit(target: &k::CaptionTarget) -> Option<&Name> {
    match target {
        k::CaptionTarget::Site(k::Site {
            unit: k::Unit::Function(function),
            ..
        }) => Some(function),
        other => other.function(),
    }
}

/// `source` with the kernel-form `spans` blanked, every byte but a newline
/// a space, so the lifted rest keeps its offsets and lines; and the words
/// the spans spell, which the front end's fresh names keep clear of.
///
/// # Errors
///
/// When a span splits a character.
pub fn blank(source: &str, spans: &[Range<usize>]) -> Result<(String, Vec<String>), String> {
    let mut blanked = source.as_bytes().to_vec();
    let mut beside = Vec::new();
    for span in spans {
        let text = source
            .get(span.clone())
            .ok_or_else(|| "a region splits a character".to_owned())?;
        beside.extend(
            text.split(|c: char| !(c.is_alphanumeric() || c == '_' || c == '$'))
                .filter(|word| !word.is_empty())
                .map(str::to_owned),
        );
        for byte in &mut blanked[span.clone()] {
            if *byte != b'\n' {
                *byte = b' ';
            }
        }
    }
    let blanked = String::from_utf8(blanked).map_err(|_| "a region splits a character")?;
    Ok((blanked, beside))
}

/// The document of a mixed module: the lifted functions as the front end
/// lowered them, and the kernel-form regions and `main` (`kernel`, as the
/// exact reader read them against `base`, which also gives their
/// signatures). The manifest lists what the code reaches.
///
/// # Errors
///
/// When a caption read names no site of the assembled document.
pub fn assemble(
    base: &k::Document,
    mut kernel: k::Document,
    kernel_captions: Vec<k::CaptionEntry>,
    lowered: Option<Lowered>,
    library: &dyn Library,
) -> Result<RegionRead, k::CaptionsRefusal> {
    // Kernel source spells no start adapter: the kernel-form units take
    // theirs from the base.
    let mut units = base.clone();
    units
        .functions
        .retain(|name, _| kernel.functions.contains_key(name));
    k::carry_start_adapters(&units, &mut kernel);
    let Some(lowered) = lowered else {
        let captions = k::Captions::new(&kernel, kernel_captions)?;
        return Ok(RegionRead {
            document: kernel,
            captions,
            map: None,
        });
    };
    let mut document = lowered.document.document().clone();
    let lifted: BTreeSet<Name> = document.functions.keys().cloned().collect();
    let mut reached = BTreeSet::new();
    let mut performed = BTreeSet::new();
    let mut nodes: Vec<k::Node<'_>> = vec![k::Node::Block(&kernel.main)];
    for (name, function) in &kernel.functions {
        nodes.push(k::Node::Block(&function.body));
        if let Some(entry) = base.entries.get(name) {
            reached.extend(entry.convention.functions().into_iter().copied());
        }
    }
    while let Some(node) = nodes.pop() {
        references(node, &mut reached, &mut performed);
        nodes.extend(node.children());
    }
    for (name, function) in &kernel.functions {
        if let Some(entry) = base.entries.get(name) {
            document.entries.insert(name.clone(), entry.clone());
        }
        document.functions.insert(name.clone(), function.clone());
    }
    document.main = kernel.main.clone();
    document.private_bindings = base.private_bindings.clone();
    let mut used: BTreeMap<_, _> = std::mem::take(&mut document.manifest.functions);
    for id in reached {
        if let Some(name) = base.manifest.functions.get(&id) {
            used.entry(id).or_insert_with(|| name.clone());
        }
    }
    document.manifest.functions = reachable(used, library);
    for effect in performed {
        if let Some(requirement) = base.manifest.effects.get(&effect) {
            document
                .manifest
                .effects
                .entry(effect)
                .or_insert_with(|| requirement.clone());
        }
    }
    require_through_bodies(&mut document.manifest, library);
    let mut entries: Vec<_> = lowered
        .captions
        .entries()
        .iter()
        .filter(|entry| unit(&entry.target).is_some_and(|name| lifted.contains(name)))
        .cloned()
        .collect();
    entries.extend(kernel_captions);
    let captions = k::Captions::new(&document, entries)?;
    Ok(RegionRead {
        document,
        captions,
        map: lowered.source_map,
    })
}

/// The library functions and effects `node` names itself.
fn references(
    node: k::Node<'_>,
    functions: &mut BTreeSet<FunctionId>,
    effects: &mut BTreeSet<k::EffectName>,
) {
    let callee = |callee: &k::Callee, functions: &mut BTreeSet<FunctionId>| {
        if let k::Callee::Library(id) = callee {
            functions.insert(*id);
        }
    };
    match node {
        k::Node::Stmt(k::Stmt::ReturnVia { policy, .. }) => {
            functions.insert(*policy);
        }
        k::Node::Stmt(k::Stmt::For { via: Some(via), .. }) => {
            functions.extend(
                [Some(via.open), via.next, via.item, via.check]
                    .into_iter()
                    .flatten(),
            );
        }
        k::Node::Action(action) => match action {
            k::Action::Call { callee: c, .. } | k::Action::Spawn { callee: c, .. } => {
                callee(c, functions);
            }
            k::Action::Perform { effect, .. } => {
                effects.insert(effect.clone());
            }
            k::Action::Await { policy, .. } => {
                functions.insert(*policy);
            }
            _ => {}
        },
        k::Node::Expr(k::Expr::Call { function, .. }) => {
            functions.insert(*function);
        }
        k::Node::Expr(k::Expr::Closure(closure)) => {
            if let Some(start) = &closure.start {
                functions.extend(start.functions().into_iter().copied());
            }
        }
        k::Node::Expr(k::Expr::Act(act)) => match &act.kind {
            k::ActKind::Call(c) | k::ActKind::Spawn(c) => callee(c, functions),
            k::ActKind::Perform { effect, .. } => {
                effects.insert(effect.clone());
            }
            k::ActKind::Await(policy) => {
                functions.insert(*policy);
            }
            _ => {}
        },
        _ => {}
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

    /// The kernel layout with `lifted` as written and every other region,
    /// and `main`, in kernel form; and where each lifted function is in it.
    ///
    /// # Errors
    ///
    /// As the dialect's kernel printer.
    fn kernel(
        &self,
        lifted: &BTreeSet<Name>,
    ) -> Result<(PrintedSource, BTreeMap<Name, Range<usize>>), PrintError>;

    /// `source` read back against the printed document. A refusal gives
    /// the offset it points at, if any.
    ///
    /// # Errors
    ///
    /// The front end's or the exact reader's refusal.
    fn read(&self, source: &str) -> Result<RegionRead, Option<usize>>;
}

/// Prints `document` with every region in `lifted` that reads back as
/// itself as authored source, and the others in kernel form: the whole
/// lifted module when it reads back as `document`, else the kernel layout
/// with the lifted regions that verify, else every region in kernel form.
///
/// # Errors
///
/// As the dialect's kernel printer, or a source map that does not fit.
pub fn print_verified(
    document: &k::Document,
    captions: Option<&k::Captions>,
    options: PrintOptions,
    mut lifted: BTreeSet<Name>,
    regions: &dyn Regions,
) -> Result<PrintedSource, PrintError> {
    let empty = k::Captions::empty(document.identity());
    let captions = captions.unwrap_or(&empty);
    let language = regions.format().language();
    if lifted.len() == document.functions.len()
        && let Some(whole) = regions.whole(&lifted)
        && whole.source.len() <= options.limits.max_bytes
    {
        match regions.read(&whole.source) {
            Ok(read) if read.matches(document, captions) => {
                return whole_printed(document, whole.source, read.map, regions.format(), options);
            }
            Ok(read) => lifted.retain(|name| read.region_matches(name, document, captions)),
            Err(at) => {
                let failed = at.and_then(|at| located(at, &whole.functions));
                lifted.retain(|name| Some(name) != failed.as_ref());
            }
        }
    }
    loop {
        if lifted.is_empty() {
            return regions.kernel(&lifted).map(|(printed, _)| printed);
        }
        let (printed, written) = regions.kernel(&lifted)?;
        match regions.read(&printed.source) {
            Ok(read) if read.matches(document, captions) => {
                return merge_maps(document, printed, read.map, &written, language);
            }
            Ok(read) => {
                let before = lifted.len();
                lifted.retain(|name| read.region_matches(name, document, captions));
                if lifted.len() == before {
                    lifted.clear();
                }
            }
            Err(at) => match at.and_then(|at| located(at, &written)) {
                Some(name) => {
                    lifted.remove(&name);
                }
                None => lifted.clear(),
            },
        }
    }
}

/// The lifted function at `offset`.
fn located(offset: usize, written: &BTreeMap<Name, Range<usize>>) -> Option<Name> {
    written
        .iter()
        .find(|(_, range)| range.contains(&offset))
        .map(|(name, _)| name.clone())
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

/// The kernel printer's map of the kernel-form regions, joined with the
/// front end's of the lifted ones.
fn merge_maps(
    document: &k::Document,
    mut printed: PrintedSource,
    lowered: Option<SourceMap>,
    written: &BTreeMap<Name, Range<usize>>,
    language: SourceLanguage,
) -> Result<PrintedSource, PrintError> {
    let Some(kernel) = printed.source_map.take() else {
        return Ok(printed);
    };
    let lowered = lowered.map(|map| map.sites).unwrap_or_default();
    let lifted = |site: &k::Site| matches!(&site.unit, k::Unit::Function(name) if written.contains_key(name));
    let mut sites: Vec<_> = kernel
        .sites
        .into_iter()
        .filter(|site| !lifted(&site.site))
        .collect();
    sites.extend(lowered.into_iter().filter(|site| lifted(&site.site)));
    printed.source_map = Some(
        SourceMap::new(document, &printed.source, language, sites)
            .map_err(|refused| PrintError::Invalid(refused.to_string()))?,
    );
    Ok(printed)
}
