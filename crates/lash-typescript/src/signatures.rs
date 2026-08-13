use lashlang::{TypeExpr, TypeField, json_schema_to_type_expr};
use serde_json::Value;

/// Renders one tool's JSON schemas as a TypeScript declaration using the
/// shared Lash type engine's schema importer.
pub fn render_tool_signature(
    name: &str,
    input_schema: &Value,
    output_schema: Option<&Value>,
) -> String {
    let input = json_schema_to_type_expr(input_schema);
    let output = output_schema.map_or(TypeExpr::Any, json_schema_to_type_expr);
    format!(
        "declare function {}(input: {}): Promise<{}>;",
        render_identifier(name),
        render_type(&input),
        render_type(&output)
    )
}

fn render_type(ty: &TypeExpr) -> String {
    match ty {
        TypeExpr::Any => "unknown".to_string(),
        TypeExpr::Str => "string".to_string(),
        TypeExpr::Int | TypeExpr::Float => "number".to_string(),
        TypeExpr::Bool => "boolean".to_string(),
        TypeExpr::Dict => "Record<string, unknown>".to_string(),
        TypeExpr::Null => "null".to_string(),
        TypeExpr::Enum(values) => values
            .iter()
            .map(|value| serde_json::to_string(value.as_str()).expect("strings serialize"))
            .collect::<Vec<_>>()
            .join(" | "),
        TypeExpr::List(item) => format!("Array<{}>", render_type(item)),
        TypeExpr::Object(fields) => render_object(fields),
        TypeExpr::Ref(name) => render_identifier(name),
        TypeExpr::Process { input, output, .. } => {
            format!("Process<{}, {}>", render_type(input), render_type(output))
        }
        TypeExpr::TriggerHandle(event) => format!("TriggerHandle<{}>", render_type(event)),
        TypeExpr::Union(items) => items
            .iter()
            .map(render_type)
            .collect::<Vec<_>>()
            .join(" | "),
    }
}

fn render_object(fields: &[TypeField]) -> String {
    if fields.is_empty() {
        return "Record<string, never>".to_string();
    }
    format!(
        "{{ {} }}",
        fields
            .iter()
            .map(|field| format!(
                "{}{}: {}",
                render_property_name(field.name.as_str()),
                if field.optional { "?" } else { "" },
                render_type(&field.ty)
            ))
            .collect::<Vec<_>>()
            .join("; ")
    )
}

fn render_identifier(name: &str) -> String {
    if is_identifier(name) {
        name.to_string()
    } else {
        format!("tool_{}", sanitize_identifier(name))
    }
}

fn render_property_name(name: &str) -> String {
    if is_identifier(name) {
        name.to_string()
    } else {
        serde_json::to_string(name).expect("strings serialize")
    }
}

fn is_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    chars
        .next()
        .is_some_and(|first| first == '_' || first == '$' || first.is_alphabetic())
        && chars
            .all(|character| character == '_' || character == '$' || character.is_alphanumeric())
}

fn sanitize_identifier(name: &str) -> String {
    let value = name
        .chars()
        .map(|character| {
            if character.is_alphanumeric() {
                character
            } else {
                '_'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        "anonymous".to_string()
    } else {
        value
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn renders_schema_through_shared_type_engine() {
        let signature = render_tool_signature(
            "search-docs",
            &json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "query": { "type": "string" }, "limit": { "type": "integer" } },
                "required": ["query"]
            }),
            Some(&json!({ "type": "array", "items": { "type": "string" } })),
        );
        assert_eq!(
            signature,
            "declare function tool_search_docs(input: { limit?: number; query: string }): Promise<Array<string>>;"
        );
    }
}
