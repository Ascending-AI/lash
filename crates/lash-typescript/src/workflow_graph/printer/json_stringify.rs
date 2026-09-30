//! Recover the complete default traversal marked by the lowerer.

use std::collections::{BTreeMap, BTreeSet};

use lashlang::{AstString, ExprFolder, StructuralRole, fold_expr_children};

use super::{Expr, Printer, TypeScriptSourceError};
use crate::workflow_graph::parse_typescript_expression;

impl Printer<'_> {
    pub(super) fn json_stringify(
        &self,
        expression: &Expr,
    ) -> Result<Option<String>, TypeScriptSourceError> {
        let Some(value) = input_binding(expression) else {
            return Ok(None);
        };
        let template = parse_typescript_expression(
            "JSON.stringify(undefined)",
            &BTreeSet::new(),
            &BTreeSet::new(),
        )
        .ok();
        let Some(template) = template else {
            return Ok(None);
        };
        if normalized_traversal(expression) != normalized_traversal(&template) {
            return Ok(None);
        }
        Ok(Some(format!("JSON.stringify({})", self.expression(value)?)))
    }
}

fn input_binding(expression: &Expr) -> Option<&Expr> {
    let Expr::Role {
        role: StructuralRole::JsonTraversal,
        expr,
    } = expression
    else {
        return None;
    };
    let Expr::Block(prefix) = expr.as_ref() else {
        return None;
    };
    let Expr::Assign { target, expr } = prefix.first()? else {
        return None;
    };
    target.is_simple().then_some(expr)
}

fn normalized_traversal(expression: &Expr) -> Option<Vec<u8>> {
    input_binding(expression)?;
    let mut traversal = expression.clone();
    let Expr::Role { expr, .. } = &mut traversal else {
        return None;
    };
    let Expr::Block(prefix) = expr.as_mut() else {
        return None;
    };
    let Expr::Assign { expr, .. } = prefix.first_mut()? else {
        return None;
    };
    // Only the input is authored. Compare all other nodes, including both
    // cycle guards, by lossless wire so signed-zero edits remain visible.
    **expr = Expr::Undefined;
    let mut bindings = StructuralBindings::default();
    bindings.collect(&traversal);
    serde_json::to_vec(&bindings.fold_expr(traversal)).ok()
}

/// Binding declarations establish the alpha-renaming order. Names never
/// determine whether an expression is a traversal or a generated binding.
#[derive(Default)]
struct StructuralBindings {
    names: BTreeMap<AstString, AstString>,
}

impl StructuralBindings {
    fn bind(&mut self, name: &AstString) {
        let index = self.names.len();
        self.names
            .entry(name.clone())
            .or_insert_with(|| format!("binding_{index}").into());
    }

    fn collect(&mut self, expression: &Expr) {
        match expression {
            Expr::Assign { target, .. } if target.is_simple() => self.bind(&target.root),
            Expr::For { binding, .. } => self.bind(binding),
            Expr::Function(function) => {
                for name in function
                    .name
                    .iter()
                    .chain(function.receiver.iter())
                    .chain(function.params.iter())
                {
                    self.bind(name);
                }
            }
            _ => {}
        }
        for child in expression.children() {
            self.collect(child);
        }
    }

    fn normalize(&self, name: &mut AstString) {
        if let Some(normalized) = self.names.get(name) {
            *name = normalized.clone();
        } else {
            *name = format!("free_{name}").into();
        }
    }
}

impl ExprFolder for StructuralBindings {
    fn fold_expr(&mut self, mut expression: Expr) -> Expr {
        match &mut expression {
            Expr::Variable(name) => self.normalize(name),
            Expr::Assign { target, .. } => self.normalize(&mut target.root),
            Expr::For { binding, .. } => self.normalize(binding),
            Expr::Function(function) => {
                for name in function
                    .name
                    .iter_mut()
                    .chain(function.receiver.iter_mut())
                    .chain(function.params.iter_mut())
                    .chain(function.captures.iter_mut())
                {
                    self.normalize(name);
                }
            }
            _ => {}
        }
        fold_expr_children(self, expression)
    }
}
