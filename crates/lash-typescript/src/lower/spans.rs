//! Carrying TypeScript source positions into the lashlang span tables.
//!
//! The lashlang diagnostic renderer prints `--> line N, column M`, the source
//! line and a caret only when the error it renders carries a `Span`, and every
//! span it can reach comes out of `Program::expression_source_spans`, which
//! addresses an expression by the path of `Expr::children()` indices that
//! reaches it from `Program::main`. The lashlang parser could fill that table
//! directly because it built each node and its span together.
//!
//! Lowering cannot: a TypeScript expression becomes a *tree* of lashlang
//! expressions, the lowerer synthesizes nodes that no source position owns,
//! and the lowered value is moved into its parent, so nothing about a node
//! survives from the moment it is built to the moment the program is finished.
//!
//! So the lowerer records a note per lowered TypeScript expression — the
//! source span, plus the shape of the lashlang subtree it produced (the root's
//! variant and the number of nodes beneath it) — and this module matches those
//! notes against the finished program. The notes are recorded in post-order
//! (children lower before their parent), so they are a *subsequence* of the
//! finished tree's post-order walk, and a single left-to-right pass assigns
//! each note the first later node whose shape it matches. A note that matches
//! nothing is dropped rather than forced onto a node: an expression with no
//! recorded span still renders, it just falls back to the enclosing span the
//! linker is already carrying.

use std::mem::Discriminant;

use lashlang::{Expr as LashExpr, ExpressionSourceSpan, Span};

use crate::SourceSpan;

/// One lowered TypeScript expression: where it came from in the source, and
/// the shape of the lashlang subtree it lowered to.
pub(super) struct SpanNote {
    span: Span,
    variant: Discriminant<LashExpr>,
    nodes: usize,
}

impl SpanNote {
    pub(super) fn new(source: SourceSpan, lowered: &LashExpr) -> Self {
        Self {
            span: Span {
                start: source.start,
                end: source.end,
            },
            variant: std::mem::discriminant(lowered),
            nodes: node_count(lowered),
        }
    }
}

fn node_count(expr: &LashExpr) -> usize {
    1 + expr.children().map(node_count).sum::<usize>()
}

struct PositionedNode {
    path: Vec<u32>,
    variant: Discriminant<LashExpr>,
    nodes: usize,
}

/// Post-order walk of `main`, collecting each node's `children()` path.
fn positions(expr: &LashExpr, path: &mut Vec<u32>, out: &mut Vec<PositionedNode>) -> usize {
    let mut nodes = 1;
    for (index, child) in expr.children().enumerate() {
        path.push(u32::try_from(index).expect("AST child index fits u32"));
        nodes += positions(child, path, out);
        path.pop();
    }
    out.push(PositionedNode {
        path: path.clone(),
        variant: std::mem::discriminant(expr),
        nodes,
    });
    nodes
}

/// Resolves `notes` against the finished `main` block.
pub(super) fn source_spans(main: &LashExpr, notes: &[SpanNote]) -> Vec<ExpressionSourceSpan> {
    let mut nodes = Vec::new();
    positions(main, &mut Vec::new(), &mut nodes);
    let mut resolved = Vec::new();
    let mut cursor = 0;
    for note in notes {
        let Some(offset) = nodes[cursor..]
            .iter()
            .position(|node| node.variant == note.variant && node.nodes == note.nodes)
        else {
            continue;
        };
        let index = cursor + offset;
        // `main` itself is not an expression the tables address.
        if !nodes[index].path.is_empty() {
            resolved.push(ExpressionSourceSpan {
                path: nodes[index].path.clone(),
                span: note.span,
            });
        }
        cursor = index + 1;
    }
    resolved.sort_by(|left, right| left.path.cmp(&right.path));
    resolved
}
