//! Turning a declared process-literal parameter type into the runtime's
//! type language.
//!
//! A process parameter's declared type is the only TypeScript annotation the
//! runtime keeps: it becomes the parameter's type in the process signature,
//! which a trigger registration is checked against before any foreground
//! effect runs. That makes the subset it accepts a durable decision, so the
//! conversion refuses rather than widens — an annotation the runtime cannot
//! carry would otherwise silently become `Any` and let a mismatched
//! registration through (FIG-3071).
//!
//! A parameter with no annotation stays `Any`, so existing programs lower
//! byte-identically.

use lashlang::{TypeExpr, TypeField};

use crate::adapter::{TypeAnnotation, TypeAnnotationField, TypeShape};
use crate::{Diagnostic, DiagnosticCode};

/// Converts one parameter's annotation, naming the process and the parameter
/// if it has to refuse.
pub(super) fn process_param_type(
    process_name: &str,
    param_name: &str,
    annotation: &TypeAnnotation,
) -> Result<TypeExpr, Diagnostic> {
    convert(annotation).map_err(|construct| {
        Diagnostic::refusal(
            DiagnosticCode::ProcessParamTypeUnsupported,
            format!(
                "process `{process_name}` parameter `{param_name}` declares {construct}, \
                 which has no durable type"
            ),
            Some(annotation.span),
        )
    })
}

/// The error is the words naming the construct, so the caller owns the whole
/// sentence and the parameter it belongs to.
fn convert(annotation: &TypeAnnotation) -> Result<TypeExpr, &'static str> {
    Ok(match &annotation.shape {
        TypeShape::Unknown => TypeExpr::Any,
        TypeShape::String => TypeExpr::Str,
        TypeShape::Number => TypeExpr::Float,
        TypeShape::Boolean => TypeExpr::Bool,
        TypeShape::Null | TypeShape::Undefined => TypeExpr::Null,
        TypeShape::StringLiteral(value) => TypeExpr::Enum(vec![value.as_str().into()]),
        TypeShape::Array(item) => TypeExpr::List(Box::new(convert(item)?)),
        TypeShape::Object(fields) => TypeExpr::Object(object_fields(fields)?),
        TypeShape::Union(items) => union(items)?,
        TypeShape::Reference(name) => TypeExpr::Ref(name.as_str().into()),
        TypeShape::Unsupported(construct) => return Err(construct),
    })
}

fn object_fields(fields: &[TypeAnnotationField]) -> Result<Vec<TypeField>, &'static str> {
    fields
        .iter()
        .map(|field| {
            Ok(TypeField {
                name: field.name.as_str().into(),
                ty: convert(&field.ty)?,
                optional: field.optional,
            })
        })
        .collect()
}

/// A union written entirely of string literals is an enumeration, which the
/// runtime carries as one type rather than as a union of single-value types —
/// the same shape `render_type` prints back into the dialect prompt.
fn union(items: &[TypeAnnotation]) -> Result<TypeExpr, &'static str> {
    let literals = items
        .iter()
        .map(|item| match &item.shape {
            TypeShape::StringLiteral(value) => Some(value.as_str().into()),
            _ => None,
        })
        .collect::<Option<Vec<_>>>();
    if let Some(values) = literals {
        return Ok(TypeExpr::Enum(values));
    }
    let variants = items.iter().map(convert).collect::<Result<Vec<_>, _>>()?;
    Ok(TypeExpr::union(variants))
}
