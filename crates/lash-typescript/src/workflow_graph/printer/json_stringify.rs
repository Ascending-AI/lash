//! Recover only the complete default traversal emitted by the lowerer.

use std::collections::BTreeSet;
use std::sync::LazyLock;

use lashlang::{AstString, ExprFolder, fold_expr_children};

use super::{Expr, Printer, TypeScriptSourceError};
use crate::lower::GENERATED_BINDING_PREFIX;
use crate::workflow_graph::parse_typescript_expression;

static DEFAULT_TRAVERSAL: LazyLock<Option<Vec<u8>>> = LazyLock::new(|| {
    let expression = parse_typescript_expression(
        "JSON.stringify(undefined)",
        &BTreeSet::new(),
        &BTreeSet::new(),
    )
    .ok()?;
    normalized_traversal(&expression)
});

impl Printer<'_> {
    pub(super) fn json_stringify(
        &self,
        expression: &Expr,
    ) -> Result<Option<String>, TypeScriptSourceError> {
        let Some((_, value)) = input_binding(expression) else {
            return Ok(None);
        };
        let Some(template) = DEFAULT_TRAVERSAL.as_ref() else {
            return Ok(None);
        };
        if normalized_traversal(expression).as_ref() != Some(template) {
            return Ok(None);
        }
        Ok(Some(format!("JSON.stringify({})", self.expression(value)?)))
    }
}

fn input_binding(expression: &Expr) -> Option<(&str, &Expr)> {
    let Expr::Block(prefix) = expression else {
        return None;
    };
    let Expr::Assign { target, expr } = prefix.first()? else {
        return None;
    };
    (target.is_simple() && target.root.ends_with("_json_input"))
        .then_some((target.root.as_str(), expr.as_ref()))
}

fn generated_index(name: &str) -> Option<(usize, &str)> {
    let (index, label) = name
        .strip_prefix(GENERATED_BINDING_PREFIX)?
        .split_once('_')?;
    Some((index.parse().ok()?, label))
}

fn normalized_traversal(expression: &Expr) -> Option<Vec<u8>> {
    let (input, _) = input_binding(expression)?;
    let (base, _) = generated_index(input)?;
    let mut traversal = expression.clone();
    let Expr::Block(prefix) = &mut traversal else {
        return None;
    };
    let Expr::Assign { expr, .. } = prefix.first_mut()? else {
        return None;
    };
    // The input is the only authored expression. Compare every other node,
    // including callback bodies and both cycle guards, against the lowerer.
    **expr = Expr::Undefined;
    // Expr equality treats signed zeros alike; their stored representations
    // and artifact identities distinguish them. Compare the lossless wire.
    serde_json::to_vec(&RelativeBindings { base }.fold_expr(traversal)).ok()
}

/// JSON temporaries are allocated together before lowering the input. Their
/// relative numbers and labels are stable even inside another JSON call.
struct RelativeBindings {
    base: usize,
}

impl RelativeBindings {
    fn normalize(&self, name: &mut AstString) {
        if let Some((index, label)) = generated_index(name)
            && let Some(relative) = index.checked_sub(self.base)
        {
            *name = format!("{GENERATED_BINDING_PREFIX}{relative}_{label}").into();
        }
    }
}

impl ExprFolder for RelativeBindings {
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
