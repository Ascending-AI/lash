//! What a front end keeps beside each statement it emits, and the three
//! site-keyed layers it makes of them once the document is whole: the
//! source map, the written table and the captions.

use std::collections::BTreeMap;

use lash_kernel_doc::Caption;
use lash_kernel_doc::CaptionEntry;
use lash_kernel_doc::Captions;
use lash_kernel_doc::CaptionsRefusal;
use lash_kernel_doc::Node;
use lash_kernel_doc::SourceLanguage;
use lash_kernel_doc::SourceMap;
use lash_kernel_doc::SourceMapRefusal;
use lash_kernel_doc::SourceSite;
use lash_kernel_doc::Span;
use lash_kernel_doc::Stmt;
use lash_vocabulary::legacy_names::Name;
use lash_vocabulary::sites::Site;
use lash_vocabulary::sites::Unit;

use crate::Written;

/// Where one emitted statement came from, kept beside it until the
/// document is whole and its sites can be counted.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Note {
    /// The source the statement serves: its own construct's, or, for
    /// scaffolding and a hoisted temporary, the construct it is part of.
    /// `None` only for code no source spells.
    pub span: Option<Span>,
    /// What the function the statement binds is, as its source wrote it.
    pub written: Option<Written>,
    /// What the source wrote of closures folded below the statement, by
    /// each closure's path from the statement.
    pub nested_written: Vec<(Vec<u32>, Written)>,
    /// The notes of the statement's blocks, in the order a walk of its
    /// children meets them.
    pub blocks: Vec<Vec<Note>>,
    /// The caption its author construct wrote, on exactly one statement the
    /// construct emits: the one holding its first action, else the last
    /// (its binding); a control's own statement. Scaffolding, hoisted
    /// temporaries and predeclarations carry none.
    pub caption: Option<Caption>,
    /// The captions of the statement's clause arms, by the arm's index in
    /// [`Self::blocks`].
    pub arm_captions: BTreeMap<usize, Caption>,
}

impl Note {
    /// The notes of code a front end makes whole, each of its statements,
    /// nested ones too, located at `span`: the construct the code serves.
    #[must_use]
    pub fn covering(statements: &[Stmt], span: Option<Span>) -> Vec<Self> {
        statements
            .iter()
            .map(|statement| {
                let mut nested = Vec::new();
                nested_blocks(
                    Node::Stmt(statement),
                    &mut Site::new(Unit::Main, Vec::new()),
                    &mut nested,
                );
                Self {
                    span,
                    blocks: nested
                        .into_iter()
                        .map(|(inner, _)| Self::covering(inner, span))
                        .collect(),
                    ..Self::default()
                }
            })
            .collect()
    }
}

/// The site-keyed layers of a lowered document.
#[derive(Clone, Debug, PartialEq)]
pub struct SiteLayers {
    pub source_map: SourceMap,
    /// What the source wrote of each function it makes, by its closure's
    /// site.
    pub written: BTreeMap<Site, Written>,
    pub captions: Captions,
}

/// A front end's notes that name no node, or a span outside its source.
#[derive(Clone, Debug, PartialEq, Eq, thiserror::Error)]
pub enum SiteLayersRefusal {
    #[error(transparent)]
    SourceMap(#[from] SourceMapRefusal),
    #[error(transparent)]
    Captions(#[from] CaptionsRefusal),
}

/// The source map of `document`, lowered from `source`, what the source
/// wrote of each function it makes, by its closure's site, and the captions
/// its notes carry.
///
/// `main` are the notes of the document's `main`, and `functions` those of
/// each declared function the source spelled; a declared function with
/// none, such as a saved function an earlier cell made, has no locations.
/// `declared` are the captions no statement's note carries: of declared
/// functions' bodies, their parameters and the fields of their types. Of
/// those, a parameter's or a field's that names nothing the document
/// declares is left out, as unattached caption syntax is trivia: a front
/// end reads the fields its source spells, and only the kernel's types say
/// which of them an entry's signature states. A statement's span locates it
/// and its action.
///
/// # Errors
///
/// The map's refusal when a span is not within `source` or a note names no
/// node, and the captions' when a note's or a function body's caption names
/// no captionable site: a front end's defect.
pub fn site_layers(
    document: &lash_kernel_doc::IdentifiedDocument,
    source: &str,
    language: SourceLanguage,
    main: &[Note],
    functions: &BTreeMap<Name, Vec<Note>>,
    declared: &[CaptionEntry],
) -> Result<SiteLayers, SiteLayersRefusal> {
    let mut layers = Layers::default();
    layers.block(&document.main, main, &Site::new(Unit::Main, Vec::new()));
    for (name, function) in &document.functions {
        if let Some(notes) = functions.get(name) {
            layers.block(
                &function.body,
                notes,
                &Site::new(Unit::Function(name.clone()), Vec::new()),
            );
        }
    }
    layers.captions.extend(
        declared
            .iter()
            .filter(|entry| entry.target.site().is_some() || entry.target.kind(document).is_some())
            .cloned(),
    );
    let source_map = SourceMap::new_identified(document, source, language, layers.sites)?;
    let captions = Captions::new_identified(document, layers.captions)?;
    for (site, written) in &mut layers.written {
        let body = site.child(0);
        written.captions = captions
            .entries()
            .iter()
            .filter_map(|entry| {
                let site = entry.target.site()?;
                (site.unit == body.unit).then_some(())?;
                let path = site.path.strip_prefix(body.path.as_slice())?;
                Some((path.to_vec(), entry.caption.clone()))
            })
            .collect();
    }
    Ok(SiteLayers {
        source_map,
        written: layers.written,
        captions,
    })
}

#[derive(Default)]
struct Layers {
    sites: Vec<SourceSite>,
    written: BTreeMap<Site, Written>,
    captions: Vec<CaptionEntry>,
}

impl Layers {
    fn block(&mut self, statements: &[Stmt], notes: &[Note], site: &Site) {
        for (index, (statement, note)) in statements.iter().zip(notes).enumerate() {
            let index = u32::try_from(index).unwrap_or(u32::MAX);
            let statement_site = site.child(index);
            if let Some(function) = &note.written
                && let Some(closure) = crate::closure_site(statement, &statement_site)
            {
                self.written.insert(closure, function.clone());
            }
            for (path, function) in &note.nested_written {
                let mut closure = statement_site.clone();
                closure.path.extend(path);
                self.written.insert(closure, function.clone());
            }
            if let Some(span) = note.span {
                self.locate(statement_site.clone(), span);
                self.actions(Node::Stmt(statement), &mut statement_site.clone(), span);
            }
            if let Some(caption) = &note.caption {
                self.captions
                    .push(CaptionEntry::site(statement_site.clone(), caption.clone()));
            }
            let mut nested = Vec::new();
            nested_blocks(
                Node::Stmt(statement),
                &mut statement_site.clone(),
                &mut nested,
            );
            for (arm, ((inner, inner_site), inner_notes)) in
                nested.into_iter().zip(&note.blocks).enumerate()
            {
                if let Some(caption) = note.arm_captions.get(&arm) {
                    self.captions
                        .push(CaptionEntry::site(inner_site.clone(), caption.clone()));
                }
                self.block(inner, inner_notes, &inner_site);
            }
        }
    }

    /// A statement owns every nested action up to its inner blocks. Those
    /// blocks carry their own source notes, including inline closure bodies.
    /// `site` is the node's own, extended and restored as the walk descends.
    fn actions(&mut self, node: Node<'_>, site: &mut Site, span: Span) {
        for (index, child) in (0u32..).zip(node.children()) {
            if matches!(child, Node::Block(_)) {
                continue;
            }
            site.path.push(index);
            if matches!(
                child,
                Node::Action(_) | Node::Expr(lash_kernel_doc::Expr::Act(_))
            ) {
                self.locate(site.clone(), span);
            }
            self.actions(child, site, span);
            site.path.pop();
        }
    }

    fn locate(&mut self, site: Site, span: Span) {
        self.sites.push(SourceSite {
            site,
            ranges: vec![span],
            implicit: None,
        });
    }
}

/// The blocks directly under a node, in the order a walk of its children
/// meets them, each with its site. `site` is the node's own, extended and
/// restored as the walk descends.
fn nested_blocks<'a>(node: Node<'a>, site: &mut Site, out: &mut Vec<(&'a [Stmt], Site)>) {
    for (index, child) in node.children().into_iter().enumerate() {
        site.path.push(u32::try_from(index).unwrap_or(u32::MAX));
        match child {
            Node::Block(inner) => out.push((inner, site.clone())),
            other => nested_blocks(other, site, out),
        }
        site.path.pop();
    }
}
