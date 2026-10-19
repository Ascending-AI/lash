//! Bounded inline spelling for the current presenters. The graph remains
//! authoritative: reference-aware presenters use its definitions directly.

use lash_tool_shape::{
    DefinitionId, ExtraKeys, SchemaDocument, SchemaShape, ShapeImportRefusal, ShapeKind,
};
use serde_json::Value;

/// Import a graph, then project its root for an inline presenter.
/// Only the presentation projection cuts recursive edges to unknown; import
/// malformed schema values return an error; resource cuts retain validation notes.
/// Named-definition presenters should consume [`SchemaDocument`] directly.
///
/// ```
/// # use lash_tool_presenter_api::inline_schema;
/// let shape = inline_schema(&serde_json::json!({"type":"string"}), 8)?;
/// assert!(matches!(shape.kind, lash_tool_shape::ShapeKind::Str));
/// # Ok::<(), lash_tool_shape::ShapeImportRefusal>(())
/// ```
///
/// # Errors
/// Returns the importer's typed refusal for an unrepresentable schema.
pub fn inline_schema(schema: &Value, max_depth: usize) -> Result<SchemaShape, ShapeImportRefusal> {
    let document = SchemaDocument::from_json_schema(schema, max_depth)?;
    Ok(expand(
        &document,
        &document.roots[0],
        max_depth,
        &mut Vec::new(),
    ))
}

/// Project a graph node for a bounded inline presenter. This leaves the
/// document unchanged and cuts recursive edges only in the returned spelling.
/// Use the document's definitions directly for a named-definition presenter.
///
/// ```
/// # use lash_tool_presenter_api::inline_shape;
/// # use lash_tool_shape::SchemaDocument;
/// let doc = SchemaDocument::from_json_schema(&serde_json::json!({"type":"string"}), 8)?;
/// let shape = inline_shape(&doc, &doc.roots[0], 8);
/// assert!(matches!(shape.kind, lash_tool_shape::ShapeKind::Str));
/// # Ok::<(), lash_tool_shape::ShapeImportRefusal>(())
/// ```
pub fn inline_shape(
    document: &SchemaDocument,
    shape: &SchemaShape,
    max_depth: usize,
) -> SchemaShape {
    expand(document, shape, max_depth, &mut Vec::new())
}

fn expand(
    document: &SchemaDocument,
    shape: &SchemaShape,
    remaining: usize,
    active: &mut Vec<DefinitionId>,
) -> SchemaShape {
    if remaining == 0 {
        let mut shape = SchemaShape::unknown();
        shape
            .constraints
            .rules
            .push("Further structure is validated by the tool.".into());
        return shape;
    }
    if let ShapeKind::Ref(id) = shape.kind {
        let mut inner = if active.contains(&id) {
            let mut shape = SchemaShape::unknown();
            shape
                .constraints
                .rules
                .push("Recursive reference is validated by the tool.".into());
            shape
        } else {
            active.push(id);
            let result = expand(
                document,
                &document.definitions[id.0].shape,
                remaining,
                active,
            );
            active.pop();
            result
        };
        inner.title = shape.title.clone().or(inner.title);
        inner.description = shape.description.clone().or(inner.description);
        inner.default = shape.default.clone().or(inner.default);
        inner.constraints = shape.constraints.clone().overlay(inner.constraints);
        return inner;
    }
    let mut result = shape.clone();
    let mut child = |shape: &SchemaShape| expand(document, shape, remaining - 1, active);
    result.kind = match &shape.kind {
        ShapeKind::List(item) => ShapeKind::List(Box::new(child(item))),
        ShapeKind::Tuple(items) => ShapeKind::Tuple(items.iter().map(&mut child).collect()),
        ShapeKind::Intersection(items) => {
            ShapeKind::Intersection(items.iter().map(&mut child).collect())
        }
        ShapeKind::Union(items) => ShapeKind::Union(items.iter().map(&mut child).collect()),
        ShapeKind::Object(object) => {
            let mut object = object.clone();
            for field in &mut object.fields {
                field.shape = child(&field.shape);
            }
            if let ExtraKeys::Open(extra) = &mut object.extra_keys {
                **extra = child(extra);
            }
            ShapeKind::Object(object)
        }
        ShapeKind::Process(Some(process)) | ShapeKind::Function(Some(process)) => {
            let mut process = process.clone();
            for param in &mut process.params {
                param.shape = child(&param.shape);
            }
            process.output = Box::new(child(&process.output));
            if matches!(shape.kind, ShapeKind::Process(_)) {
                ShapeKind::Process(Some(process))
            } else {
                ShapeKind::Function(Some(process))
            }
        }
        other => other.clone(),
    };
    result
}
