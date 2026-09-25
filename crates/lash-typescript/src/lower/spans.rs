//! Carrying TypeScript source positions into the lashlang span table.
//!
//! Lowering changes tree shape: it synthesizes nodes, moves authored values
//! into wrappers, and lifts process literals into declarations. Matching the
//! finished tree by expression shape loses provenance when two expressions are
//! identical and cannot distinguish `main` from declaration roots.
//!
//! Instead, lowering temporarily wraps each sourced node in a private label.
//! The wrapper moves with the node through every lowering transform. Once the
//! complete program exists, one mutable walk removes the wrappers and records
//! their root-qualified [`AstPath`]s in `Program::spans`. The private labels
//! never leave this module and therefore change neither the IR nor identity.

use std::collections::{BTreeMap, BTreeSet};

use lashlang::{AstPath, AstRoot, Declaration, Expr as LashExpr, LabelMetadata, Program, Span};

use crate::SourceSpan;

const MARKER_DESCRIPTION: &str = "\0s";

#[derive(Default)]
pub(super) struct SpanMarkers {
    spans: Vec<Span>,
}

impl SpanMarkers {
    pub(super) fn annotate(&mut self, source: SourceSpan, expression: LashExpr) -> LashExpr {
        let marker = self.spans.len().to_string();
        self.spans.push(Span {
            start: source.start,
            end: source.end,
        });
        LashExpr::LabelAnnotated {
            label: LabelMetadata {
                title: marker.into(),
                description: Some(MARKER_DESCRIPTION.into()),
            },
            expr: Box::new(expression),
        }
    }

    pub(super) fn resolve(self, program: &mut Program) {
        let mut resolved = BTreeMap::new();
        let mut resolved_markers = BTreeSet::new();
        for (index, declaration) in program.declarations.iter_mut().enumerate() {
            let Ok(index) = u32::try_from(index) else {
                unreachable!("the source bound keeps the declaration count within u32")
            };
            let root = AstRoot::Declaration(index);
            match declaration {
                Declaration::Process(process) => extract(
                    &mut process.body,
                    AstPath {
                        root,
                        steps: Vec::new(),
                    },
                    &self.spans,
                    &mut resolved_markers,
                    &mut resolved,
                ),
                Declaration::Function(function) => extract(
                    &mut function.body,
                    AstPath {
                        root,
                        steps: Vec::new(),
                    },
                    &self.spans,
                    &mut resolved_markers,
                    &mut resolved,
                ),
                Declaration::Type(_) => {}
            }
        }
        extract(
            &mut program.main,
            AstPath::main(Vec::new()),
            &self.spans,
            &mut resolved_markers,
            &mut resolved,
        );
        debug_assert!(
            (0..self.spans.len()).all(|marker| resolved_markers.contains(&marker)),
            "every allocated source marker is accounted for"
        );
        program.spans = resolved;
    }
}

pub(super) fn unmarked(mut expression: &LashExpr) -> &LashExpr {
    while let LashExpr::LabelAnnotated { label, expr } = expression {
        if label.description.as_deref() != Some(MARKER_DESCRIPTION) {
            break;
        }
        expression = expr;
    }
    expression
}

pub(super) fn unmarked_mut(expression: &mut LashExpr) -> &mut LashExpr {
    let is_marker = matches!(
        expression,
        LashExpr::LabelAnnotated { label, .. }
            if label.description.as_deref() == Some(MARKER_DESCRIPTION)
    );
    if is_marker {
        let LashExpr::LabelAnnotated { expr, .. } = expression else {
            unreachable!("the marker predicate matched")
        };
        return unmarked_mut(expr);
    }
    expression
}

fn extract(
    expression: &mut LashExpr,
    path: AstPath,
    markers: &[Span],
    resolved_markers: &mut BTreeSet<usize>,
    resolved: &mut BTreeMap<AstPath, Span>,
) {
    while let LashExpr::LabelAnnotated { label, expr } = expression {
        if label.description.as_deref() != Some(MARKER_DESCRIPTION) {
            break;
        }
        let Some((marker, span)) = label
            .title
            .parse::<usize>()
            .ok()
            .and_then(|index| markers.get(index).copied().map(|span| (index, span)))
        else {
            break;
        };
        let inner = std::mem::replace(expr, Box::new(LashExpr::Undefined));
        *expression = *inner;
        resolved_markers.insert(marker);
        resolved.insert(path.clone(), span);
    }

    for (index, child) in expression.children_mut().enumerate() {
        let Ok(index) = u32::try_from(index) else {
            unreachable!("the source bound keeps a node's child count within u32")
        };
        extract(
            child,
            path.child(index),
            markers,
            resolved_markers,
            resolved,
        );
    }
}

/// The source position a lowered TypeScript expression is reported at.
///
/// Only the forms a diagnostic points at carry one: a call, a member access
/// and an await. Everything else inherits the nearest enclosing span the
/// linker is already carrying, which is what the lashlang parser's tables did
/// for a sub-expression it recorded no span for.
pub(super) fn source_span(expr: &super::Expr) -> Option<SourceSpan> {
    match expr {
        super::Expr::Call { span, .. }
        | super::Expr::Member { span, .. }
        | super::Expr::Await { span, .. } => Some(*span),
        super::Expr::Ident(_, span) => *span,
        _ => None,
    }
}
