//! What a front end keeps beside each statement it emits, and the two
//! site-keyed layers it makes of them once the document is whole: the
//! source map and the written table.

use std::collections::BTreeMap;

use lash_kernel_doc::{
    Name, Node, Site, SourceLanguage, SourceMap, SourceMapRefusal, SourceSite, Span, Stmt, Unit,
};

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
                    &Site::new(Unit::Main, Vec::new()),
                    &mut nested,
                );
                Self {
                    span,
                    written: None,
                    nested_written: Vec::new(),
                    blocks: nested
                        .into_iter()
                        .map(|(inner, _)| Self::covering(inner, span))
                        .collect(),
                }
            })
            .collect()
    }
}

/// The source map of `document`, lowered from `source`, and what the
/// source wrote of each function it makes, by its closure's site.
///
/// `main` are the notes of the document's `main`, and `functions` those of
/// each declared function the source spelled; a declared function with
/// none, such as a saved function an earlier cell made, has no locations.
/// A statement's span locates it and its action.
///
/// # Errors
///
/// The map's refusal when a span is not within `source` or a note names no
/// node: a front end's defect.
pub fn site_layers(
    document: &lash_kernel_doc::IdentifiedDocument,
    source: &str,
    language: SourceLanguage,
    main: &[Note],
    functions: &BTreeMap<Name, Vec<Note>>,
) -> Result<(SourceMap, BTreeMap<Site, Written>), SourceMapRefusal> {
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
    let map = SourceMap::new_identified(document, source, language, layers.sites)?;
    Ok((map, layers.written))
}

#[derive(Default)]
struct Layers {
    sites: Vec<SourceSite>,
    written: BTreeMap<Site, Written>,
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
                for (child, index) in Node::Stmt(statement).children().into_iter().zip(0u32..) {
                    if matches!(
                        child,
                        Node::Action(_) | Node::Expr(lash_kernel_doc::Expr::Act(_))
                    ) {
                        self.locate(statement_site.child(index), span);
                    }
                }
            }
            let mut nested = Vec::new();
            nested_blocks(Node::Stmt(statement), &statement_site, &mut nested);
            for ((inner, inner_site), inner_notes) in nested.into_iter().zip(&note.blocks) {
                self.block(inner, inner_notes, &inner_site);
            }
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
/// meets them, each with its site.
fn nested_blocks<'a>(node: Node<'a>, site: &Site, out: &mut Vec<(&'a [Stmt], Site)>) {
    for (index, child) in node.children().into_iter().enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        let child_site = site.child(index);
        match child {
            Node::Block(inner) => out.push((inner, child_site)),
            other => nested_blocks(other, &child_site, out),
        }
    }
}
