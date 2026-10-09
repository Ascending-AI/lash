//! A process declaration's TypeScript spelling: the `const` binding of an
//! `async` arrow the lowerer's process wrapper comes from, and the type
//! annotations on its parameters and settled output.

use lash_vm::{Expr, ProcessDecl, ProcessLiteralExpr, TypeExpr};

use super::{
    Printed, Printer, TypeScriptSourceError, label_comment, property_name, string_literal,
};

impl<'p> Printer<'p> {
    /// Re-sugar a lowered process declaration into its authored arrow.
    ///
    /// A process is an uncalled `const`-bound `async` arrow (FIG-2999): the
    /// binding is the name a reader sees, and the declaration's own name is a
    /// lift digest that no authored source spells.
    pub(super) fn define_process(
        &self,
        binding: &str,
        process: &ProcessDecl,
        bound: &mut Vec<String>,
    ) -> Printed {
        let body = process_run_body(process).ok_or(TypeScriptSourceError::Unrepresentable {
            kind: "a process body that is not the lowerer's process wrapper",
        })?;
        let params = authored_params(process)
            .iter()
            .map(|param| self.process_param(param))
            .collect::<Result<Vec<_>, _>>()?;
        let mut out = String::new();
        if let Some(label) = &process.label {
            out.push_str(&label_comment(label)?);
            out.push('\n');
        }
        out.push_str(&format!(
            "const {} = async ({}){} => ",
            self.binding_identifier("process binding", binding)?,
            params.join(", "),
            process_return_annotation(super::super::authored_return_type(&process.origin))?,
        ));
        let mut run_bound = authored_params(process)
            .iter()
            .map(|param| param.name.to_string())
            .collect::<Vec<_>>();
        out.push_str(&self.rooted_block(body, 0, &mut run_bound)?);
        out.push_str(";\n");
        bound.push(binding.to_string());
        Ok(out)
    }

    /// A process parameter with the annotation its declared type lowers
    /// from, so a typed parameter keeps its type through a re-admission.
    pub(super) fn process_param(&self, param: &lash_vm::ProcessParam) -> Printed {
        let name = self.identifier("process parameter", param.name.as_str())?;
        Ok(match type_annotation(&param.ty)? {
            Some(annotation) => format!("{name}: {annotation}"),
            None => name,
        })
    }
}

/// A process's authored parameters: a lifted literal's hidden start arguments
/// are not among them.
pub(super) fn authored_params(process: &ProcessDecl) -> &[lash_vm::ProcessParam] {
    let hidden = match &process.origin {
        lash_vm::ProcessOrigin::Lifted { hidden_params, .. } => *hidden_params as usize,
        lash_vm::ProcessOrigin::Declared => 0,
    };
    &process.params[..process.params.len().saturating_sub(hidden)]
}

/// The authored `run` body inside a process-wrapper body.
pub(super) fn process_run_body(process: &ProcessDecl) -> Option<&Expr> {
    crate::lower::wrapped_run_body(&process.body)
}

/// The authored `run` body inside a process *literal*'s wrapper.
pub(crate) fn process_literal_run_body(literal: &ProcessLiteralExpr) -> Option<&Expr> {
    crate::lower::wrapped_run_body(&literal.body)
}

/// Prints the settled output as an async return annotation.
pub(super) fn process_return_annotation(ty: Option<&TypeExpr>) -> Printed {
    match ty {
        Some(ty) => Ok(format!(
            ": Promise<{}>",
            type_annotation(ty)?.unwrap_or_else(|| "unknown".to_string())
        )),
        None => Ok(String::new()),
    }
}

/// The TypeScript annotation a process parameter type lowers from, or `None`
/// for `Any`, which an unannotated parameter lowers to. It inverts the
/// lowering's annotation conversion; a type no annotation lowers to is
/// refused rather than widened.
fn type_annotation(ty: &TypeExpr) -> Result<Option<String>, TypeScriptSourceError> {
    fn annotation(ty: &TypeExpr) -> Result<String, TypeScriptSourceError> {
        Ok(match ty {
            TypeExpr::Any => "unknown".to_string(),
            TypeExpr::Str => "string".to_string(),
            TypeExpr::Float => "number".to_string(),
            TypeExpr::Bool => "boolean".to_string(),
            TypeExpr::Null => "null".to_string(),
            TypeExpr::Enum(values) => values
                .iter()
                .map(|value| string_literal(value.as_str()))
                .collect::<Vec<_>>()
                .join(" | "),
            TypeExpr::List(item) => format!("Array<{}>", annotation(item)?),
            TypeExpr::Object(fields) => format!(
                "{{ {} }}",
                fields
                    .iter()
                    .map(|field| {
                        Ok(format!(
                            "{}{}: {}",
                            property_name(field.name.as_str()),
                            if field.optional { "?" } else { "" },
                            annotation(&field.ty)?
                        ))
                    })
                    .collect::<Result<Vec<_>, TypeScriptSourceError>>()?
                    .join("; ")
            ),
            TypeExpr::Union(items) => items
                .iter()
                .map(annotation)
                .collect::<Result<Vec<_>, _>>()?
                .join(" | "),
            TypeExpr::Ref(name) => name.to_string(),
            _ => {
                return Err(TypeScriptSourceError::Unrepresentable {
                    kind: "a process parameter type no TypeScript annotation lowers to",
                });
            }
        })
    }
    match ty {
        TypeExpr::Any => Ok(None),
        ty => annotation(ty).map(Some),
    }
}
