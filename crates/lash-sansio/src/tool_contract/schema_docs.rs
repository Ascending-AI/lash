//! The compact contract's spelling of a [`SchemaShape`].
//!
//! The compact contract is the catalog-facing projection of a tool contract:
//! a one-line signature and one row per field. It spells the shared shape
//! model in a terse, language-neutral notation (`record{query: str}`,
//! `list[int]`); a dialect that shows a model its own syntax spells the same
//! model itself.

use super::*;

pub fn schema_for<T>() -> serde_json::Value
where
    T: schemars::JsonSchema,
{
    serde_json::to_value(schemars::schema_for!(T)).unwrap_or_else(|_| serde_json::json!({}))
}

/// A shape's type in compact notation.
pub(crate) fn compact_type(shape: &SchemaShape) -> String {
    match &shape.kind {
        ShapeKind::Unknown => "any".to_string(),
        ShapeKind::Null => "null".to_string(),
        ShapeKind::Bool => "bool".to_string(),
        ShapeKind::Int => "int".to_string(),
        ShapeKind::Float => "float".to_string(),
        ShapeKind::Str => "str".to_string(),
        ShapeKind::Literals(values) => format!(
            "enum[{}]",
            values
                .iter()
                .map(display_value)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ShapeKind::List(item) => format!("list[{}]", compact_type(item)),
        ShapeKind::Tuple(items) => format!(
            "tuple[{}]",
            items
                .iter()
                .map(compact_type)
                .collect::<Vec<_>>()
                .join(", ")
        ),
        ShapeKind::Object(object) => match compact_members(object) {
            members if members.is_empty() => "record".to_string(),
            members => format!("record{{{}}}", members.join(", ")),
        },
        ShapeKind::Union(members) => {
            let mut labels = Vec::<String>::new();
            for label in members.iter().map(compact_type) {
                if !labels.contains(&label) {
                    labels.push(label);
                }
            }
            labels.join(" | ")
        }
        ShapeKind::Named(name) => name.clone(),
        ShapeKind::Process(None) => "process".to_string(),
        ShapeKind::Process(Some(signature)) => format!(
            "process[({}) -> {}]",
            signature
                .params
                .iter()
                .map(|param| format!("{}: {}", param.name, compact_type(&param.shape)))
                .collect::<Vec<_>>()
                .join(", "),
            compact_type(&signature.output)
        ),
        ShapeKind::Handle(payload) => format!("handle[{}]", compact_type(payload)),
    }
}

/// An object's members: each field as `name: type`, then `...` when the
/// schema says extra keys are allowed.
fn compact_members(object: &ObjectShape) -> Vec<String> {
    let mut members = object
        .fields
        .iter()
        .map(|field| {
            format!(
                "{}{}: {}",
                field.name,
                if field.required { "" } else { "?" },
                compact_type(&field.shape)
            )
        })
        .collect::<Vec<_>>();
    if let ExtraKeys::Open(extra) = &object.extra_keys {
        match extra.kind {
            ShapeKind::Unknown if members.is_empty() => {}
            ShapeKind::Unknown => members.push("...".to_string()),
            _ => members.push(format!("...: {}", compact_type(extra))),
        }
    }
    members
}

/// The argument record of a signature: `{ query: str, limit?: int <= 10 = 5 }`.
pub(crate) fn compact_arguments(input: &SchemaShape) -> String {
    let mut members = input
        .fields()
        .iter()
        .map(|field| compact_field(&field.name, field.required, &field.shape))
        .collect::<Vec<_>>();
    if let ShapeKind::Object(ObjectShape {
        extra_keys: ExtraKeys::Open(_),
        fields,
    }) = &input.kind
        && !fields.is_empty()
    {
        members.push("...".to_string());
    }
    if members.is_empty() {
        "{}".to_string()
    } else {
        format!("{{ {} }}", members.join(", "))
    }
}

/// One field with its notes: `limit?: int >= 1 <= 20 = 5`.
fn compact_field(name: &str, required: bool, shape: &SchemaShape) -> String {
    let mut out = format!(
        "{name}{}: {}",
        if required { "" } else { "?" },
        compact_type(shape)
    );
    for note in shape.constraints.notes() {
        out.push(' ');
        out.push_str(&note);
    }
    if let Some(default) = &shape.default {
        out.push_str(" = ");
        out.push_str(&display_value(default));
    }
    out
}

/// A field row as the compact contract stores it. `name_key` is `name` for a
/// parameter and `path` for a return field.
pub(crate) fn compact_row(row: &ShapeRow, name_key: &str) -> serde_json::Value {
    let mut out = serde_json::Map::new();
    out.insert(name_key.to_string(), serde_json::json!(row.path));
    out.insert(
        "type".to_string(),
        serde_json::json!(compact_type(&row.shape)),
    );
    out.insert("required".to_string(), serde_json::json!(row.required));
    if let Some(description) = &row.shape.description {
        out.insert("description".to_string(), serde_json::json!(description));
    }
    out.insert(
        "signature".to_string(),
        serde_json::json!(compact_field(&row.path, row.required, &row.shape)),
    );
    serde_json::Value::Object(out)
}

/// Inline one schema node whose `$ref` carries sibling keywords.
///
/// The reference chain is expanded against `root` and the node's own siblings
/// are merged over the resolved definition, so a sibling `description` wins
/// over the definition's, per JSON Schema draft annotation semantics. Returns
/// `None` when the reference is not a resolvable local pointer, leaving the
/// caller to report it rather than emit a schema the provider will reject.
/// References whose expansion was cut short by a cycle are reported in
/// `cycles`.
pub(crate) fn resolve_ref_node_with_siblings(
    root: &serde_json::Value,
    node: &serde_json::Value,
    cycles: &mut Vec<String>,
) -> Option<serde_json::Value> {
    let mut resolving = Vec::new();
    let resolved = resolve_schema_ref_value(root, node, &mut resolving, cycles);
    if resolved.get("$ref").is_some() {
        return None;
    }
    Some(resolved)
}

fn resolve_schema_ref_value(
    root: &serde_json::Value,
    schema: &serde_json::Value,
    resolving: &mut Vec<String>,
    cycles: &mut Vec<String>,
) -> serde_json::Value {
    match schema {
        serde_json::Value::Object(map) => {
            if let Some(reference) = map.get("$ref").and_then(serde_json::Value::as_str)
                && let Some(pointer) = reference.strip_prefix('#')
            {
                if resolving.iter().any(|active| active == reference) {
                    cycles.push(reference.to_string());
                    return serde_json::json!({});
                }
                if let Some(target) = root.pointer(pointer) {
                    resolving.push(reference.to_string());
                    let mut resolved = resolve_schema_ref_value(root, target, resolving, cycles);
                    resolving.pop();

                    let sibling_count = map.keys().filter(|key| key.as_str() != "$ref").count();
                    if sibling_count == 0 {
                        return resolved;
                    }
                    if let serde_json::Value::Object(resolved_map) = &mut resolved {
                        for (key, value) in map {
                            if key == "$ref" {
                                continue;
                            }
                            resolved_map.insert(
                                key.clone(),
                                resolve_schema_ref_value(root, value, resolving, cycles),
                            );
                        }
                        return resolved;
                    }
                }
            }

            serde_json::Value::Object(
                map.iter()
                    .map(|(key, value)| {
                        (
                            key.clone(),
                            resolve_schema_ref_value(root, value, resolving, cycles),
                        )
                    })
                    .collect(),
            )
        }
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .iter()
                .map(|value| resolve_schema_ref_value(root, value, resolving, cycles))
                .collect(),
        ),
        other => other.clone(),
    }
}

pub(crate) fn compact_doc_line(value: &serde_json::Value) -> Option<String> {
    let signature = value.get("signature")?.as_str()?.trim();
    if signature.is_empty() {
        return None;
    }
    let description = value
        .get("description")
        .and_then(serde_json::Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    Some(match description {
        Some(description) => format!("- `{signature}` — {description}"),
        None => format!("- `{signature}`"),
    })
}

pub(crate) fn compact_examples(examples: &[String], limit: usize) -> Vec<String> {
    examples
        .iter()
        .map(|example| example.trim())
        .filter(|example| !example.is_empty())
        .take(limit)
        .map(|example| {
            if example.chars().count() <= COMPACT_TOOL_EXAMPLE_CHAR_LIMIT {
                return example.to_string();
            }
            let mut out = example
                .chars()
                .take(COMPACT_TOOL_EXAMPLE_CHAR_LIMIT.saturating_sub(3))
                .collect::<String>();
            out.push_str("...");
            out
        })
        .collect()
}

fn display_value(value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "null".to_string(),
        serde_json::Value::Bool(v) => v.to_string(),
        serde_json::Value::Number(v) => v.to_string(),
        serde_json::Value::String(v) => format!("{v:?}"),
        _ => serde_json::to_string(value).unwrap_or_else(|_| "null".to_string()),
    }
}
