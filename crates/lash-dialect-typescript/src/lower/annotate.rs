//! Turns the notes kept beside each statement into annotations and the
//! written table, both keyed by site, once the document is whole.

use std::collections::BTreeMap;

use lash_kernel_dialect::Written;
use lash_kernel_doc::{Annotations, Document, Node, NodeAnnotation, Site, Stmt, Unit};

use super::{Lowering, Note};
use crate::{Diagnostic, DiagnosticCode};

/// The annotation layer of `document`, and what the source wrote of each
/// function it makes, by the site of the function's closure.
pub(super) fn annotations(
    document: &Document,
    notes: &[Note],
    declared_notes: &BTreeMap<lash_kernel_doc::Name, Vec<Note>>,
    source: &str,
) -> Lowering<(Annotations, BTreeMap<Site, Written>)> {
    let identity = document
        .identity()
        .map_err(|error| Diagnostic::new(DiagnosticCode::InvalidAst, error.to_string(), None))?;
    let mut annotations = Annotations::new(identity);
    annotations.dialect = Some("typescript".to_string());
    annotations.source = Some(source.to_string());
    let mut written = BTreeMap::new();
    block(
        &document.main,
        notes,
        &Site::new(Unit::Main, Vec::new()),
        &mut annotations.nodes,
        &mut written,
    );
    for (name, notes) in declared_notes {
        if let Some(function) = document.functions.get(name) {
            block(
                &function.body,
                notes,
                &Site::new(Unit::Function(name.clone()), Vec::new()),
                &mut annotations.nodes,
                &mut written,
            );
        }
    }
    Ok((annotations, written))
}

fn block(
    statements: &[Stmt],
    notes: &[Note],
    site: &Site,
    out: &mut Vec<NodeAnnotation>,
    written: &mut BTreeMap<Site, Written>,
) {
    for (index, (statement, note)) in statements.iter().zip(notes).enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        let statement_site = site.child(index);
        if let Some(function) = &note.written
            && let Some(closure) = lash_kernel_dialect::closure_site(statement, &statement_site)
        {
            written.insert(closure, function.clone());
        }
        if let Some(span) = note.span {
            let mut data = BTreeMap::new();
            data.insert(
                "span".to_string(),
                serde_json::json!([span.start, span.end]),
            );
            out.push(NodeAnnotation {
                site: statement_site.clone(),
                label: None,
                data,
            });
        }
        let mut nested = Vec::new();
        nested_blocks(Node::Stmt(statement), &statement_site, &mut nested);
        for ((inner, inner_site), inner_notes) in nested.into_iter().zip(&note.blocks) {
            block(inner, inner_notes, &inner_site, out, written);
        }
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
