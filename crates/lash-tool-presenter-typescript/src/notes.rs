use lash_vocabulary::tool_shape::ExtraKeys;
use lash_vocabulary::tool_shape::SchemaShape;
use lash_vocabulary::tool_shape::ShapeKind;

use crate::{is_identifier, is_reserved_word};

/// Path-only annotations, stopping at reference edges. Pass `input`, `output`
/// or the allocated definition name to preserve each annotation's owner.
/// Union branch paths remain distinct. No row repeats a type expression.
///
/// ```rust,ignore
/// let rows = note_rows(&document.roots[0], "input");
/// // - `input.limit` (>= 1, default 10) — Page size.
/// ```
pub fn note_rows(shape: &SchemaShape, path: &str) -> Vec<String> {
    let mut rows = Vec::new();
    collect(shape, path, &mut rows);
    rows
}

fn collect(shape: &SchemaShape, path: &str, rows: &mut Vec<String>) {
    if shape.has_notes() {
        let mut row = format!("- `{}`", path.replace('`', "\\u0060"));
        let mut notes = shape.constraints.notes();
        if let Some(default) = &shape.default {
            notes.push(format!("default {default}"));
        }
        if !notes.is_empty() {
            row.push_str(&format!(" ({})", notes.join(", ")));
        }
        if let Some(description) = &shape.description {
            row.push_str(&format!(" — {description}"));
        }
        rows.push(row);
    }
    match &shape.kind {
        ShapeKind::Object(object) => {
            for field in &object.fields {
                let key = if is_identifier(&field.name) && !is_reserved_word(&field.name) {
                    format!(".{name}", name = field.name)
                } else {
                    format!("[{}]", serde_json::Value::String(field.name.clone()))
                };
                collect(&field.shape, &format!("{path}{key}"), rows);
            }
            if let ExtraKeys::Open(extra) = &object.extra_keys {
                collect(extra, &format!("{path}.*"), rows);
            }
        }
        ShapeKind::List(item) => collect(item, &format!("{path}[]"), rows),
        ShapeKind::Tuple(items) => {
            for (index, item) in items.iter().enumerate() {
                collect(item, &format!("{path}[{index}]"), rows);
            }
        }
        ShapeKind::Union(items) => {
            for (item, arm) in items.iter().zip(shape.union_arm_paths(path)) {
                collect(item, &arm, rows);
            }
        }
        ShapeKind::Intersection(items) => {
            for item in items {
                collect(item, path, rows);
            }
        }
        ShapeKind::Process(Some(process)) | ShapeKind::Function(Some(process)) => {
            for param in &process.params {
                collect(&param.shape, &format!("{path}({})", param.name), rows);
            }
            collect(&process.output, &format!("{path}.return"), rows);
        }
        _ => {}
    }
}
