use lash_kernel_value::tool_shape::ExtraKeys;
use lash_kernel_value::tool_shape::SchemaShape;
use lash_kernel_value::tool_shape::ShapeKind;

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
    collect(shape, &mut path.to_owned(), &mut rows);
    rows
}

#[expect(clippy::expect_used, reason = "JSON field names always serialize")]
fn collect(shape: &SchemaShape, path: &mut String, rows: &mut Vec<String>) {
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
                let end = path.len();
                if is_identifier(&field.name) && !is_reserved_word(&field.name) {
                    path.push('.');
                    path.push_str(&field.name);
                } else {
                    path.push('[');
                    path.push_str(
                        &serde_json::to_string(&field.name).expect("field name serializes"),
                    );
                    path.push(']');
                }
                collect(&field.shape, path, rows);
                path.truncate(end);
            }
            if let ExtraKeys::Open(extra) = &object.extra_keys {
                suffix(extra, path, ".*", rows);
            }
        }
        ShapeKind::List(item) => suffix(item, path, "[]", rows),
        ShapeKind::Tuple(items) => {
            for (index, item) in items.iter().enumerate() {
                suffix(item, path, &format!("[{index}]"), rows);
            }
        }
        ShapeKind::Union(items) => {
            for (item, mut arm) in items.iter().zip(shape.union_arm_paths(path)) {
                collect(item, &mut arm, rows);
            }
        }
        ShapeKind::Intersection(items) => {
            for item in items {
                collect(item, path, rows);
            }
        }
        ShapeKind::Process(Some(process)) | ShapeKind::Function(Some(process)) => {
            for param in &process.params {
                suffix(&param.shape, path, &format!("({})", param.name), rows);
            }
            suffix(&process.output, path, ".return", rows);
        }
        _ => {}
    }
}

fn suffix(shape: &SchemaShape, path: &mut String, suffix: &str, rows: &mut Vec<String>) {
    let end = path.len();
    path.push_str(suffix);
    collect(shape, path, rows);
    path.truncate(end);
}
