//! Turns the notes kept beside each statement into annotations keyed by
//! site, once the document is whole.

use std::collections::BTreeMap;

use lash_kernel_doc::{Annotations, Document, Label, Node, NodeAnnotation, Site, Stmt, Unit};

use super::{Lowering, Note};
use crate::{Diagnostic, DiagnosticCode};

pub(super) fn annotations(
    document: &Document,
    notes: &[Note],
    source: &str,
) -> Lowering<Annotations> {
    let identity = document
        .identity()
        .map_err(|error| Diagnostic::new(DiagnosticCode::InvalidAst, error.to_string(), None))?;
    let mut annotations = Annotations::new(identity);
    annotations.dialect = Some("typescript".to_string());
    annotations.source = Some(source.to_string());
    block(
        &document.main,
        notes,
        &Site::new(Unit::Main, Vec::new()),
        &mut annotations.nodes,
    );
    Ok(annotations)
}

fn block(statements: &[Stmt], notes: &[Note], site: &Site, out: &mut Vec<NodeAnnotation>) {
    for (index, (statement, note)) in statements.iter().zip(notes).enumerate() {
        let index = u32::try_from(index).unwrap_or(u32::MAX);
        let statement_site = site.child(index);
        if note.span.is_some() || note.label.is_some() || note.written.is_some() {
            let mut data = BTreeMap::new();
            if let Some(written) = &note.written {
                data.insert(lash_kernel_dialect::WRITTEN.to_string(), written.clone());
            }
            if let Some(span) = note.span {
                data.insert(
                    "span".to_string(),
                    serde_json::json!([span.start, span.end]),
                );
            }
            out.push(NodeAnnotation {
                site: statement_site.clone(),
                label: note.label.as_ref().map(|label| Label {
                    title: label.title.clone(),
                    description: label.description.clone(),
                }),
                data,
            });
        }
        let mut nested = Vec::new();
        nested_blocks(Node::Stmt(statement), &statement_site, &mut nested);
        for ((inner, inner_site), inner_notes) in nested.into_iter().zip(&note.blocks) {
            block(inner, inner_notes, &inner_site, out);
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
